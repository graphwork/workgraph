//! `wg spawn-task` — the single entry point that turns a task-id
//! into a live handler process.
//!
//! See `docs/design/sessions-as-identity.md` for the full model.
//! This command:
//!   1. Looks up the task in the graph
//!   2. Resolves its executor type, chat session, and role
//!   3. Dispatches to the right handler command via a per-executor
//!      adapter
//!   4. `exec()`s into the child so stdio passes through cleanly —
//!      the PTY embedding in `wg tui` just spawns `wg spawn-task`
//!      and gets the handler's output as its own.
//!
//! Adapters live inline here (one match arm per executor). Native
//! execs into `wg nex`; Claude execs into `wg claude-handler`
//! (the standalone Claude CLI ↔ chat/*.jsonl bridge). Codex /
//! Gemini / Amplifier are still stubs — they error cleanly with a
//! "not yet implemented" message when selected.

use std::path::Path;

use anyhow::{Context, Result, anyhow};

use workgraph::graph::Task;

/// Dispatch table for what handler to run for a task. Parsed from
/// the task's executor hint (config override) or defaults to native.
#[derive(Clone, Debug)]
pub enum HandlerSpec {
    Native {
        chat_ref: String,
        role: Option<String>,
        resume: bool,
        model: Option<String>,
        endpoint: Option<String>,
    },
    Claude {
        chat_ref: String,
        model: Option<String>,
    },
    Codex {
        chat_ref: String,
        model: Option<String>,
    },
    Gemini {
        chat_ref: String,
    },
    Amplifier {
        chat_ref: String,
    },
}

impl HandlerSpec {
    /// Render the command line we'd exec, for preview / dry-run.
    pub fn command_preview(&self) -> String {
        match self {
            Self::Native {
                chat_ref,
                role,
                resume,
                model,
                endpoint,
            } => {
                let mut s = format!("wg nex --chat {}", chat_ref);
                if *resume {
                    s.push_str(" --resume");
                }
                if let Some(r) = role {
                    s.push_str(&format!(" --role {}", r));
                }
                if let Some(m) = model {
                    s.push_str(&format!(" -m {}", m));
                }
                if let Some(e) = endpoint {
                    s.push_str(&format!(" -e {}", e));
                }
                s
            }
            Self::Claude { chat_ref, model } => {
                let mut s = format!("wg claude-handler --chat {}", chat_ref);
                if let Some(m) = model {
                    s.push_str(&format!(" -m {}", m));
                }
                s
            }
            Self::Codex { chat_ref, model } => {
                let mut s = format!("wg codex-handler --chat {}", chat_ref);
                if let Some(m) = model {
                    s.push_str(&format!(" -m {}", m));
                }
                s
            }
            Self::Gemini { chat_ref } => format!("gemini [TODO: adapter for session={}]", chat_ref),
            Self::Amplifier { chat_ref } => {
                format!("wg amplifier-run {} [TODO]", chat_ref)
            }
        }
    }
}

/// The entry point called from `main.rs` for `Commands::SpawnTask`.
pub fn run(
    workgraph_dir: &Path,
    task_id: &str,
    role_override: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    let graph_path = workgraph_dir.join("graph.jsonl");
    // A missing graph.jsonl is NOT a fatal error for spawn-task:
    // the daemon needs to spawn coordinator-0 on startup before any
    // tasks exist (and before the graph file has even been created
    // on first run). We treat "no graph file" the same as "empty
    // graph" and fall through to the synthesized-task branch. Any
    // OTHER load error (malformed JSONL, permissions, etc.) still
    // bails.
    let graph = if graph_path.exists() {
        workgraph::parser::load_graph(&graph_path)
            .with_context(|| format!("load graph at {:?}", graph_path))?
    } else {
        workgraph::graph::WorkGraph::new()
    };
    let found = graph.tasks().find(|t| t.id == task_id).cloned();
    let task = match found {
        Some(t) => t,
        None if is_coordinator_id(task_id) => {
            // Coordinator sessions can exist without a graph task —
            // the daemon auto-spawns coordinator-0 at startup before
            // any `CreateCoordinator` IPC fires, and older flows
            // drove `wg nex --chat coordinator-N` without a graph
            // entry at all. Synthesize a minimal task so handler
            // resolution still works.
            Task {
                id: task_id.to_string(),
                title: task_id.to_string(),
                ..Default::default()
            }
        }
        None => return Err(anyhow!("no such task: {}", task_id)),
    };

    let spec = resolve_handler(workgraph_dir, &task, role_override)?;

    if dry_run {
        println!("{}", spec.command_preview());
        return Ok(());
    }

    dispatch(&spec, workgraph_dir)
}

/// Figure out what kind of handler to spawn for this task, given
/// config + task-specific overrides.
pub fn resolve_handler(
    workgraph_dir: &Path,
    task: &Task,
    role_override: Option<&str>,
) -> Result<HandlerSpec> {
    let config = workgraph::config::Config::load_or_default(workgraph_dir);

    // chat_ref convention: task id IS the chat alias, until Phase 5
    // migration swaps to `.chat-<uuid>`. Exception: `.coordinator-N`
    // task ids map to the existing `coordinator-N` chat alias the
    // daemon registers via `register_coordinator_session` — so IPC
    // writers (`wg chat --coordinator N`) and the handler land on
    // the SAME underlying chat dir. Without this, the handler would
    // use a fresh `chat/.coordinator-N/` dir that no other code
    // writes to, and the coordinator's inbox would appear empty.
    let chat_ref = if let Some(n) = task.id.strip_prefix(".coordinator-") {
        format!("coordinator-{}", n)
    } else {
        task.id.clone()
    };

    // Role: coordinator tasks get `--role coordinator`. Caller
    // override wins. `.compact-*`, `.assign-*`, etc. inherit no
    // special role — they're just task-agent runs.
    let role = role_override.map(|s| s.to_string()).or_else(|| {
        if task.id.starts_with(".coordinator-") {
            Some("coordinator".to_string())
        } else {
            None
        }
    });

    // Executor pick: task-level model hints override, else config
    // default. For Phase 2 we only fully support Native; other
    // executors compile but print a "not yet implemented" error at
    // dispatch (Phase 7 fills them in).
    let executor_kind = pick_executor(&config, task);

    // Resolve model: task.model (user's per-task override) wins,
    // then fall back to config.coordinator.model (user's project-level setting).
    // This ensures `wg spawn-task .coordinator-N` picks up the configured model
    // even when the graph task doesn't have a model field set.
    let effective_model = task.model.clone().or_else(|| config.coordinator.model.clone());

    // Resume if the session journal exists on disk — same rule
    // `wg nex` uses internally. Route through the registry so
    // aliases (`coordinator-0`, `0`) resolve to the UUID dir.
    let chat_dir = workgraph::chat::chat_dir_for_ref(workgraph_dir, &chat_ref);
    let journal_exists = chat_dir.join("conversation.jsonl").exists();

    Ok(match executor_kind {
        ExecutorKind::Native => HandlerSpec::Native {
            chat_ref,
            role,
            resume: journal_exists,
            model: effective_model.clone(),
            endpoint: task.endpoint.clone(),
        },
        ExecutorKind::Claude => HandlerSpec::Claude {
            chat_ref,
            model: effective_model.clone(),
        },
        ExecutorKind::Codex => HandlerSpec::Codex {
            chat_ref,
            model: effective_model,
        },
        ExecutorKind::Gemini => HandlerSpec::Gemini { chat_ref },
        ExecutorKind::Amplifier => HandlerSpec::Amplifier { chat_ref },
    })
}

fn is_coordinator_id(task_id: &str) -> bool {
    task_id
        .strip_prefix(".coordinator-")
        .is_some_and(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExecutorKind {
    Native,
    Claude,
    Codex,
    Gemini,
    Amplifier,
}

fn pick_executor(config: &workgraph::config::Config, task: &Task) -> ExecutorKind {
    // Precedence:
    //   1. `WG_EXECUTOR_TYPE` env var — daemon sets this per-coordinator
    //      so a Claude coordinator in the same graph as a native one
    //      routes correctly even if the global default is different.
    //   2. Task-level `executor` field (reserved; unused today).
    //   3. Coordinator config default (`config.coordinator.executor`).
    //   4. "native".
    let env_kind = std::env::var("WG_EXECUTOR_TYPE").ok();
    let label = env_kind
        .as_deref()
        .or(config.coordinator.executor.as_deref())
        .unwrap_or("native");
    match label.to_ascii_lowercase().as_str() {
        "native" | "nex" => ExecutorKind::Native,
        "claude" => ExecutorKind::Claude,
        "codex" => ExecutorKind::Codex,
        "gemini" => ExecutorKind::Gemini,
        "amplifier" => ExecutorKind::Amplifier,
        _ => {
            eprintln!(
                "\x1b[33m[spawn-task] unknown executor {:?} for task {}, defaulting to native\x1b[0m",
                label, task.id
            );
            ExecutorKind::Native
        }
    }
}

/// Exec into the handler process. This REPLACES the current process
/// (via `execvp`) on Unix so stdio passes through cleanly — the PTY
/// parent sees the handler's bytes directly.
fn dispatch(spec: &HandlerSpec, _workgraph_dir: &Path) -> Result<()> {
    match spec {
        HandlerSpec::Native {
            chat_ref,
            role,
            resume,
            model,
            endpoint,
        } => dispatch_native(
            chat_ref,
            role.as_deref(),
            *resume,
            model.as_deref(),
            endpoint.as_deref(),
        ),
        HandlerSpec::Claude { chat_ref, model } => dispatch_claude(chat_ref, model.as_deref()),
        HandlerSpec::Codex { chat_ref, model } => dispatch_codex(chat_ref, model.as_deref()),
        HandlerSpec::Gemini { .. } => Err(anyhow!(
            "gemini adapter not yet implemented (Phase 7). Use --executor native for now."
        )),
        HandlerSpec::Amplifier { .. } => Err(anyhow!(
            "amplifier adapter via spawn-task not yet implemented (Phase 7). \
             Use the existing service-level amplifier dispatch for now."
        )),
    }
}

fn dispatch_codex(chat_ref: &str, model: Option<&str>) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let self_exe =
            std::env::current_exe().context("resolve current exe for spawn-task dispatch")?;
        let mut cmd = std::process::Command::new(&self_exe);
        cmd.arg("codex-handler").arg("--chat").arg(chat_ref);
        if let Some(m) = model {
            cmd.arg("-m").arg(m);
        }
        let err = cmd.exec();
        Err(anyhow!("exec wg codex-handler failed: {}", err))
    }
    #[cfg(not(unix))]
    {
        let _ = (chat_ref, model);
        Err(anyhow!(
            "spawn-task dispatch not yet supported on this platform"
        ))
    }
}

fn dispatch_claude(chat_ref: &str, model: Option<&str>) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let self_exe =
            std::env::current_exe().context("resolve current exe for spawn-task dispatch")?;
        let mut cmd = std::process::Command::new(&self_exe);
        cmd.arg("claude-handler").arg("--chat").arg(chat_ref);
        // Coordinator role is implicit for `coordinator-*` refs; pass
        // explicit role if the caller set one via role_override.
        if let Some(m) = model {
            cmd.arg("-m").arg(m);
        }
        let err = cmd.exec();
        Err(anyhow!("exec wg claude-handler failed: {}", err))
    }
    #[cfg(not(unix))]
    {
        let _ = (chat_ref, model);
        Err(anyhow!(
            "spawn-task dispatch not yet supported on this platform"
        ))
    }
}

fn dispatch_native(
    chat_ref: &str,
    role: Option<&str>,
    resume: bool,
    model: Option<&str>,
    endpoint: Option<&str>,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let self_exe =
            std::env::current_exe().context("resolve current exe for spawn-task dispatch")?;
        let mut cmd = std::process::Command::new(&self_exe);
        cmd.arg("nex").arg("--chat").arg(chat_ref);
        if resume {
            cmd.arg("--resume");
        }
        if let Some(r) = role {
            cmd.arg("--role").arg(r);
        }
        if let Some(m) = model {
            cmd.arg("-m").arg(m);
        }
        if let Some(e) = endpoint {
            cmd.arg("-e").arg(e);
        }
        // Clean handoff — exec replaces us, child inherits stdio.
        let err = cmd.exec();
        // exec() only returns on error.
        Err(anyhow!("exec wg nex failed: {}", err))
    }
    #[cfg(not(unix))]
    {
        // Fallback on non-Unix: spawn + wait + propagate exit code.
        let _ = (chat_ref, role, resume, model, endpoint);
        Err(anyhow!(
            "spawn-task dispatch not yet supported on this platform"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn mktask(id: &str) -> Task {
        Task {
            id: id.to_string(),
            title: id.to_string(),
            ..Default::default()
        }
    }

    // These tests expect Native handler; isolate from WG_EXECUTOR_TYPE env var
    // which the coordinator daemon sets per-agent.
    #[test]
    #[serial]
    fn coordinator_task_gets_coordinator_role() {
        let saved = std::env::var("WG_EXECUTOR_TYPE").ok();
        unsafe { std::env::remove_var("WG_EXECUTOR_TYPE") };
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".workgraph")).unwrap();
        let task = mktask(".coordinator-0");
        let spec = resolve_handler(dir.path(), &task, None).unwrap();
        if let Some(v) = saved {
            unsafe { std::env::set_var("WG_EXECUTOR_TYPE", v) };
        }
        match spec {
            HandlerSpec::Native { role, .. } => {
                assert_eq!(role, Some("coordinator".to_string()));
            }
            _ => panic!("expected Native handler"),
        }
    }

    #[test]
    #[serial]
    fn non_coordinator_task_gets_no_role() {
        let saved = std::env::var("WG_EXECUTOR_TYPE").ok();
        unsafe { std::env::remove_var("WG_EXECUTOR_TYPE") };
        let dir = tempfile::tempdir().unwrap();
        let task = mktask("my-task");
        let spec = resolve_handler(dir.path(), &task, None).unwrap();
        if let Some(v) = saved {
            unsafe { std::env::set_var("WG_EXECUTOR_TYPE", v) };
        }
        match spec {
            HandlerSpec::Native { role, .. } => {
                assert!(role.is_none(), "regular task should not have a role");
            }
            _ => panic!("expected Native handler"),
        }
    }

    #[test]
    #[serial]
    fn role_override_wins() {
        let saved = std::env::var("WG_EXECUTOR_TYPE").ok();
        unsafe { std::env::remove_var("WG_EXECUTOR_TYPE") };
        let dir = tempfile::tempdir().unwrap();
        let task = mktask(".coordinator-0");
        let spec = resolve_handler(dir.path(), &task, Some("evaluator")).unwrap();
        if let Some(v) = saved {
            unsafe { std::env::set_var("WG_EXECUTOR_TYPE", v) };
        }
        match spec {
            HandlerSpec::Native { role, .. } => {
                assert_eq!(role, Some("evaluator".to_string()));
            }
            _ => panic!("expected Native handler"),
        }
    }

    #[test]
    #[serial]
    fn resume_true_when_journal_exists() {
        let saved = std::env::var("WG_EXECUTOR_TYPE").ok();
        unsafe { std::env::remove_var("WG_EXECUTOR_TYPE") };
        let dir = tempfile::tempdir().unwrap();
        let task = mktask("have-journal");
        let chat = dir.path().join("chat").join(&task.id);
        std::fs::create_dir_all(&chat).unwrap();
        std::fs::write(chat.join("conversation.jsonl"), b"").unwrap();
        let spec = resolve_handler(dir.path(), &task, None).unwrap();
        if let Some(v) = saved {
            unsafe { std::env::set_var("WG_EXECUTOR_TYPE", v) };
        }
        match spec {
            HandlerSpec::Native { resume, .. } => assert!(resume),
            _ => panic!(),
        }
    }

    #[test]
    #[serial]
    fn resume_false_when_fresh() {
        let saved = std::env::var("WG_EXECUTOR_TYPE").ok();
        unsafe { std::env::remove_var("WG_EXECUTOR_TYPE") };
        let dir = tempfile::tempdir().unwrap();
        let task = mktask("fresh-task");
        let spec = resolve_handler(dir.path(), &task, None).unwrap();
        if let Some(v) = saved {
            unsafe { std::env::set_var("WG_EXECUTOR_TYPE", v) };
        }
        match spec {
            HandlerSpec::Native { resume, .. } => assert!(!resume),
            _ => panic!(),
        }
    }

    #[test]
    fn command_preview_has_chat_flag() {
        let spec = HandlerSpec::Native {
            chat_ref: "foo".into(),
            role: Some("coordinator".into()),
            resume: true,
            model: None,
            endpoint: None,
        };
        let p = spec.command_preview();
        assert!(p.contains("--chat foo"));
        assert!(p.contains("--resume"));
        assert!(p.contains("--role coordinator"));
    }

    #[test]
    #[serial]
    fn spawn_task_passes_model_to_claude_handler() {
        let saved = std::env::var("WG_EXECUTOR_TYPE").ok();
        unsafe { std::env::set_var("WG_EXECUTOR_TYPE", "claude") };
        let dir = tempfile::tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir.join("config.toml").parent().unwrap()).unwrap();

        let mut task = mktask("test-task");
        task.model = Some("claude:opus".to_string());
        let spec = resolve_handler(wg_dir, &task, None).unwrap();

        if let Some(v) = saved {
            unsafe { std::env::set_var("WG_EXECUTOR_TYPE", v) };
        } else {
            unsafe { std::env::remove_var("WG_EXECUTOR_TYPE") };
        }

        let preview = spec.command_preview();
        match spec {
            HandlerSpec::Claude { model, .. } => {
                assert_eq!(
                    model,
                    Some("claude:opus".to_string()),
                    "task.model should pass through to HandlerSpec"
                );
            }
            _ => panic!("expected Claude handler"),
        }
        assert!(
            preview.contains("-m claude:opus"),
            "dry-run should include --model flag: {}",
            preview
        );
    }

    #[test]
    #[serial]
    fn spawn_task_falls_back_to_config_model() {
        let saved = std::env::var("WG_EXECUTOR_TYPE").ok();
        unsafe { std::env::set_var("WG_EXECUTOR_TYPE", "claude") };
        let dir = tempfile::tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::write(
            wg_dir.join("config.toml"),
            b"[coordinator]\nmodel = \"claude:opus\"\n",
        )
        .unwrap();

        let task = mktask(".coordinator-0");
        assert!(task.model.is_none(), "synthesized task has no model");
        let spec = resolve_handler(wg_dir, &task, None).unwrap();

        if let Some(v) = saved {
            unsafe { std::env::set_var("WG_EXECUTOR_TYPE", v) };
        } else {
            unsafe { std::env::remove_var("WG_EXECUTOR_TYPE") };
        }

        let preview = spec.command_preview();
        match spec {
            HandlerSpec::Claude { model, .. } => {
                assert_eq!(
                    model,
                    Some("claude:opus".to_string()),
                    "should fall back to config.coordinator.model when task.model is None"
                );
            }
            _ => panic!("expected Claude handler"),
        }
        assert!(
            preview.contains("-m claude:opus"),
            "dry-run should include config model: {}",
            preview
        );
    }

    #[test]
    #[serial]
    fn user_pinned_dated_id_passes_through_unchanged() {
        let saved = std::env::var("WG_EXECUTOR_TYPE").ok();
        unsafe { std::env::set_var("WG_EXECUTOR_TYPE", "claude") };
        let dir = tempfile::tempdir().unwrap();

        let mut task = mktask("pinned-task");
        task.model = Some("claude:claude-opus-4-6".to_string());
        let spec = resolve_handler(dir.path(), &task, None).unwrap();

        if let Some(v) = saved {
            unsafe { std::env::set_var("WG_EXECUTOR_TYPE", v) };
        } else {
            unsafe { std::env::remove_var("WG_EXECUTOR_TYPE") };
        }

        match spec {
            HandlerSpec::Claude { model, .. } => {
                assert_eq!(
                    model,
                    Some("claude:claude-opus-4-6".to_string()),
                    "user-pinned dated ID should pass through unchanged"
                );
            }
            _ => panic!("expected Claude handler"),
        }
    }

    #[test]
    #[serial]
    fn task_model_wins_over_config_model() {
        let saved = std::env::var("WG_EXECUTOR_TYPE").ok();
        unsafe { std::env::set_var("WG_EXECUTOR_TYPE", "claude") };
        let dir = tempfile::tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::write(
            wg_dir.join("config.toml"),
            b"[coordinator]\nmodel = \"claude:sonnet\"\n",
        )
        .unwrap();

        let mut task = mktask("override-task");
        task.model = Some("claude:opus".to_string());
        let spec = resolve_handler(wg_dir, &task, None).unwrap();

        if let Some(v) = saved {
            unsafe { std::env::set_var("WG_EXECUTOR_TYPE", v) };
        } else {
            unsafe { std::env::remove_var("WG_EXECUTOR_TYPE") };
        }

        match spec {
            HandlerSpec::Claude { model, .. } => {
                assert_eq!(
                    model,
                    Some("claude:opus".to_string()),
                    "task.model should win over config.coordinator.model"
                );
            }
            _ => panic!("expected Claude handler"),
        }
    }
}

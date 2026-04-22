//! Executor configuration system for spawning agents.
//!
//! Provides configuration loading and template variable substitution for
//! executor configs stored in `.workgraph/executors/<name>.toml`.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::agency;
use crate::context_scope::ContextScope;
use crate::graph::Task;

// --- Prompt section constants for scope-based assembly ---

/// Required Workflow section: wg log/artifact/done/fail instructions + Important bullets.
/// Contains {{task_id}} placeholders for variable substitution.
pub const REQUIRED_WORKFLOW_SECTION: &str = "\
## Required Workflow

You MUST use these commands to track your work:

0. **Check for messages and reply** (BEFORE any other work):
   ```bash
   wg msg read {{task_id}} --agent $WG_AGENT_ID
   ```
   For EACH message, reply with what you'll do about it:
   ```bash
   wg msg send {{task_id}} \"Acknowledged — will fix the prefix on line 42\"
   ```
   Unreplied messages = incomplete task. This is not optional.

1. **Log progress** as you work (helps recovery if interrupted):
   ```bash
   wg log {{task_id}} \"Starting implementation...\"
   wg log {{task_id}} \"Completed X, now working on Y\"
   ```
   If you received messages in step 0, reply to them too (`wg msg send`).

2. **Record artifacts** if you create/modify files:
   ```bash
   wg artifact {{task_id}} path/to/file
   ```

3. **Validate your work** before marking done:
   - **Check task-specific criteria first:** Run `wg show {{task_id}}` and look for a **Verification Required** section or a **## Validation** section in the description. Those criteria are your primary acceptance test — address every item.
   - **Code tasks:** Run `cargo build` and `cargo test` (or the project's equivalent). Fix any failures.
   - **Research/docs tasks:** Re-read the task description and verify your output addresses every requirement. Check that referenced files and links exist.
   - **All tasks:** Log your validation results:
     ```bash
     wg log {{task_id}} \"Validated: task-specific criteria met\"
     wg log {{task_id}} \"Validated: cargo build + cargo test pass\"
     ```

4. **Commit and push** if you modified files:
   - Run `cargo build` and `cargo test` BEFORE committing — never commit broken code
   - Stage ONLY your files (never `git add -A`) and commit with a descriptive message:
     ```bash
     git add <your-files> && git commit -m \"feat: <description> ({{task_id}})\"
     git push
     ```
   - Log the commit hash:
     ```bash
     wg log {{task_id}} \"Committed: $(git rev-parse --short HEAD) — pushed to remote\"
     ```

5. **Check messages AGAIN and reply** (BEFORE marking done — this is a completion gate):
   ```bash
   wg msg read {{task_id}} --agent $WG_AGENT_ID
   ```
   Reply to ALL new messages before proceeding:
   ```bash
   wg msg send {{task_id}} \"Done — applied the requested changes in commit abc123\"
   ```
   If you skip replies, the task is incomplete. Do NOT mark done with unreplied messages.

6. **Complete the task** when done:
   ```bash
   wg done {{task_id}}
   wg done {{task_id}} --converged  # Use this if task has loop edges and work is complete
   ```

7. **Mark as failed** ONLY after genuine attempt:
   You MUST attempt the actual work before calling `wg fail`. Explaining why something \
is hard is NOT the same as attempting it. If the task involves fixing code — try fixing it. \
If it involves writing code — write the code. Only use `wg fail` when you have tried and \
hit a genuine blocker (missing API access, circular dependency, external system down). \
'The verification seems hard to satisfy' is NOT a valid failure reason — attempt the work \
and let verification tell you if it succeeded.
   ```bash
   wg fail {{task_id}} --reason \"What I tried and what specifically blocked me\"
   ```

## Anti-Pattern: Explain-and-Bail
DO NOT: Read the task → write an explanation of why it's hard → `wg fail`
DO: Read the task → attempt the work → if genuinely stuck after trying, `wg fail` with what you tried
The system has retry logic and model escalation. A failed attempt with partial progress \
is more valuable than a lengthy explanation of why you didn't try.

## Important
- Run `wg log` commands BEFORE doing work to track progress
- Validate BEFORE running `wg done`
- Commit and push your changes BEFORE running `wg done`
- Run `wg done` BEFORE you finish responding
- If the task description is unclear, do your best interpretation\n";

/// Research Hints section: encourages agents to investigate before implementing.
/// Added as part of TB heartbeat orchestration (Condition G Phase 3).
pub const RESEARCH_HINTS_SECTION: &str = "\
## Research Before Implementing

Before writing code, understand the problem:
- Read all referenced files and test cases
- If the task involves unfamiliar technology, search for documentation in the workspace
- Check existing patterns in the codebase (grep for similar implementations)
- Read error messages carefully — they often contain the fix
- For build systems (CMake, Cython, Cargo): check for existing config files first\n";

/// Graph Patterns section: vocabulary, golden rule, subtask guidance, cycle awareness.
pub const GRAPH_PATTERNS_SECTION: &str = "\
## Graph Patterns (see docs/AGENT-GUIDE.md for details)

**Vocabulary:** pipeline (A\u{2192}B\u{2192}C), diamond (A\u{2192}[B,C,D]\u{2192}E), scatter-gather (heterogeneous reviewers of same artifact), loop (A\u{2192}B\u{2192}C\u{2192}A with `--max-iterations`).

**Golden rule: same files = sequential edges.** NEVER parallelize tasks that modify the same files \u{2014} one will overwrite the other. When unsure, default to pipeline.

**Cycles (back-edges):** Workgraph is a directed graph, NOT a DAG. For repeating workflows \
(cleanup\u{2192}commit\u{2192}verify, write\u{2192}review, etc.), create ONE cycle with `--max-iterations` \
instead of duplicating tasks for each pass. Use `wg done --converged` to stop the cycle \
when no more changes are needed. If you are inside a cycle, check `wg show` for your \
`loop_iteration` and evaluate whether the work has converged before deciding to iterate or stop.

**When creating subtasks:**
- Always include an integrator task at join points: `wg add \"Integrate\" --after worker-a,worker-b`
- List each worker's file scope in the task description
- Run `wg quickstart` for full command reference

**After code changes:** Run `cargo install --path .` to update the global binary.\n";

/// Reusable Workflow Functions section: wg func list/apply/show.
pub const REUSABLE_FUNCTIONS_SECTION: &str = "\
## Reusable Workflow Functions
- `wg func list` \u{2014} discover reusable workflow patterns extracted from past tasks
- `wg func apply <id> --input key=value` \u{2014} instantiate a function to create pre-wired tasks
- `wg func show <id>` \u{2014} view function details and required inputs\n";

/// Critical warning about using wg CLI instead of built-in tools.
/// Contains {{task_id}} placeholders for variable substitution.
pub const CRITICAL_WG_CLI_SECTION: &str = "\
## CRITICAL: Use wg CLI, NOT built-in tools
- You MUST use `wg` CLI commands for ALL task management
- NEVER use built-in TaskCreate, TaskUpdate, TaskList, or TaskGet tools \u{2014} they are a completely separate system that does NOT interact with workgraph
- If you need to create subtasks: `wg add \"title\" --after {{task_id}}`
- To check task status: `wg show <task-id>`
- To list tasks: `wg list`\n";

/// System awareness preamble for full scope (~300 tokens).
const SYSTEM_AWARENESS_PREAMBLE: &str = "\
## System Awareness

You are working within **workgraph**, a task orchestration system. Key concepts:

- **Coordinator**: A daemon (`wg service start`) that polls for ready tasks and spawns agents.
- **Agency**: An evolutionary identity system with roles, motivations, and agents. Agents are assigned to tasks based on skills, performance, and fit.
- **Cycles/Loops**: Tasks can form cycles with `--max-iterations`. Use `wg done --converged` when a cycle's work is complete.
- **Trace Functions**: Reusable workflow patterns (`wg func list/apply/show`) that can instantiate pre-wired task subgraphs.
- **Context Scopes**: Agents receive different amounts of context (clean < task < graph < full) based on task requirements.\n";

/// Ethos section: the philosophical dimension of working within a living graph.
/// Injected at task+ scope to encourage autopoietic behavior.
/// Contains {{task_id}} placeholders for variable substitution.
pub const ETHOS_SECTION: &str = "\
## The Graph is Alive

You are not isolated. The graph is a shared medium — artifacts you write are read by other agents, \
tasks you create get dispatched to other agents. You are one node in a living system.

**Your job is not just to complete your task.** It is to leave the system better than you found it \
and to grow the graph where it needs growing:
- **Task too large?** Decompose it — fan out independent parts as parallel subtasks, \
add a synthesis task to integrate the results
- **Found a bug or missing doc?** `wg add \"Fix: ...\" --after {{task_id}} -d \"Found while working on {{task_id}}\"`
- **Prerequisite missing?** Create a blocking task: `wg add \"Prereq: ...\" && wg add \"{{task_id}}\" --after prereq-id`
- **Follow-up needed?** `wg add \"Verify: ...\" --after {{task_id}}`

The coordinator dispatches anything you add. You don't need permission.

**The loop:** spec \u{2192} implement \u{2192} verify \u{2192} improve \u{2192} spec. \
You may be any node. Use `wg context` to see what came before. \
Use `wg add` to create what comes next.\n";

/// Autopoietic guidance section: concrete task decomposition patterns and guardrails.
/// Injected at task+ scope to teach agents when and how to create subtasks.
/// Contains {{task_id}}, {{max_child_tasks}}, and {{max_task_depth}} placeholders.
pub const AUTOPOIETIC_GUIDANCE: &str = "\
## Task Decomposition

Fanout is a tool, not a default. Always attempt direct implementation first. \
The coordinator will dispatch any subtasks you create automatically.

### DEFAULT: Implement directly
Start by doing the work yourself. Only switch to decomposition after assessing complexity.

### Fan out when:
- 3+ independent files/components need changes that can genuinely run in parallel
- You hit context pressure: re-reading files you already read, losing track of changes
- The task has natural parallelism (e.g., 3 separate test files, N independent modules)
- You discover a bug, missing doc, or needed refactor outside your scope

### Stay inline when:
- The task is straightforward, even if it touches multiple files sequentially
- Each step depends on the previous (sequential work doesn't parallelize)
- Simple fixes, config changes, small features
- The task is hard but single-scope — difficulty alone is NOT a reason to decompose
- Decomposition overhead would exceed the work itself

### If you decompose:
- Each subtask MUST list its file scope — **NO two subtasks may modify the same file**
- Subtask descriptions should include \"Implement directly — do not decompose further\"
- ALWAYS include an integrator at join points: \
`wg add 'Integrate' --after part-a,part-b`
- ALWAYS use `--after {{task_id}}` for dependencies
- Log your decision: `wg log {{task_id}} \"FANOUT_DECISION: decompose — <reason>\"`

### How to decompose
- **Fan out parallel work**: \
`wg add 'Part A' --after {{task_id}}` and `wg add 'Part B' --after {{task_id}}`
- **Create a synthesis task**: After fan-out, add an integrator: \
`wg add 'Integrate results' --after part-a,part-b`
- **Pipeline decomposition**: \
`wg add 'Step 1' --after {{task_id}} && wg add 'Step 2' --after step-1`
- **Bug/issue found**: \
`wg add 'Fix: ...' --after {{task_id}} -d 'Found while working on {{task_id}}'`

### Include validation criteria in subtasks
Every code subtask description MUST include a `## Validation` section with concrete acceptance criteria. \
Use `--validation llm` (preferred) so an independent LLM evaluator checks the criteria, \
or `--verify 'cargo test <name>'` when a specific shell check suffices:
```bash
wg add 'Implement auth endpoint' --after {{task_id}} \
  --validation llm \
  -d '## Description
Add POST /auth/token endpoint.

## Validation
- [ ] Failing test written first: test_auth_rejects_expired_token
- [ ] Implementation makes the test pass
- [ ] cargo test passes with no regressions'
```

### Guardrails
- You can create up to **{{max_child_tasks}}** subtasks per session (configurable via `wg config`)
- Task chains have a maximum depth of **{{max_task_depth}}** levels
- Always include an integrator at join points — don't leave parallel work unmerged

### When NOT to decompose
- The task is small and well-scoped (just do it)
- Decomposition overhead exceeds the work itself
- The subtasks would all modify the same files (serialize instead)\n";

// --- Adaptive decomposition intelligence ---

/// Task complexity classification for decomposition guidance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskComplexity {
    /// Single-step task: implement directly without decomposition.
    Atomic,
    /// Multi-step task: benefits from structured decomposition.
    MultiStep,
}

/// Signal words/phrases that indicate an atomic (single-step) task.
const ATOMIC_SIGNALS: &[&str] = &[
    "single function",
    "fix bug in",
    "one file",
    "write a regex",
    "rename",
    "typo",
    "fix typo",
    "update comment",
    "change the",
    "add a field",
    "remove the",
    "delete the",
    "bump version",
    "simple fix",
    "small change",
    "one-liner",
    "hotfix",
    "quick fix",
    "minor tweak",
];

/// Signal words/phrases that indicate a multi-step task.
const MULTI_STEP_SIGNALS: &[&str] = &[
    "build pipeline",
    "configure + test",
    "configure and test",
    "multiple files",
    "install",
    "set up",
    "setup",
    "end-to-end",
    "e2e",
    "integrate",
    "migration",
    "refactor",
    "implement feature",
    "build a",
    "create a system",
    "design and implement",
    "pipeline",
    "workflow",
    "multi-step",
    "several steps",
    "phases",
    "stage 1",
    "step 1",
    "then",
    "after that",
    "followed by",
    "next,",
    "finally,",
    "first,",
    "second,",
    "third,",
];

/// Classify a task as atomic or multi-step based on description signals.
///
/// Uses a scoring heuristic: counts signal matches in the description
/// (case-insensitive). Multi-step wins ties since the cost of unnecessary
/// guidance is lower than the cost of missing decomposition structure.
pub fn classify_task_complexity(description: &str) -> TaskComplexity {
    let lower = description.to_lowercase();
    let atomic_score: usize = ATOMIC_SIGNALS.iter().filter(|s| lower.contains(*s)).count();
    let multi_score: usize = MULTI_STEP_SIGNALS
        .iter()
        .filter(|s| lower.contains(*s))
        .count();

    // Also check for structural signals: bullet lists with 3+ items suggest multi-step
    let bullet_count = lower
        .lines()
        .filter(|l| {
            let trimmed = l.trim();
            trimmed.starts_with("- [ ]")
                || trimmed.starts_with("- ")
                || trimmed.starts_with("* ")
                || trimmed.starts_with("1.")
                || trimmed.starts_with("2.")
                || trimmed.starts_with("3.")
        })
        .count();
    let multi_score = if bullet_count >= 3 {
        multi_score + 1
    } else {
        multi_score
    };

    if atomic_score > multi_score {
        TaskComplexity::Atomic
    } else if multi_score > 0 {
        TaskComplexity::MultiStep
    } else {
        // Default: if no signals matched, lean toward multi-step for
        // longer descriptions (>200 chars) and atomic for short ones
        if lower.len() > 200 {
            TaskComplexity::MultiStep
        } else {
            TaskComplexity::Atomic
        }
    }
}

/// Decomposition template: pipeline pattern (A -> B -> C).
const DECOMP_TEMPLATE_PIPELINE: &str = "\
#### Pipeline (sequential steps)
When work must proceed in order (e.g., parse -> transform -> write):
```bash
wg add 'Step 1: Parse input' --after {{task_id}} --validation llm
wg add 'Step 2: Transform data' --after step-1-parse-input --validation llm
wg add 'Step 3: Write output' --after step-2-transform-data --validation llm
```
Each step depends on the previous. Use `--validation llm` so the evaluator checks the `## Validation` block, or `--verify 'cargo test test_name'` if a specific shell check suffices.";

/// Decomposition template: fan-out-merge pattern (A -> [B,C,D] -> E).
const DECOMP_TEMPLATE_FAN_OUT: &str = "\
#### Fan-out-merge (parallel work + integration)
When work has independent parts that converge (e.g., implement N modules):
```bash
wg add 'Part A: Module X' --after {{task_id}} --validation llm
wg add 'Part B: Module Y' --after {{task_id}} --validation llm
wg add 'Part C: Module Z' --after {{task_id}} --validation llm
wg add 'Integrate modules' --after part-a-module-x,part-b-module-y,part-c-module-z \\
  --validation llm
```
**CRITICAL:** The integration task (`--after` all parts) merges the work. Never leave parallel tasks unmerged.";

/// Decomposition template: iterate-until-pass pattern (loop).
const DECOMP_TEMPLATE_ITERATE: &str = "\
#### Iterate-until-pass (refinement loop)
When work requires multiple passes (e.g., optimize -> benchmark -> optimize):
```bash
wg add 'Refine implementation' --after {{task_id}} --max-iterations 3 --validation llm
```
Use `wg done --converged` when the work meets criteria. Use `wg fail` if a pass doesn't work so the cycle restarts.";

/// Build adaptive decomposition guidance based on task classification.
///
/// When `decomp_guidance` is enabled in config, this replaces the static
/// `AUTOPOIETIC_GUIDANCE` with task-specific advice:
/// - ATOMIC tasks get a "implement directly" hint
/// - MULTI-STEP tasks get classification explanation + decomposition templates
///
/// The function still includes all standard guardrails and validation guidance.
pub fn build_decomposition_guidance(
    task_description: &str,
    task_id: &str,
    max_child_tasks: u32,
    max_task_depth: u32,
) -> String {
    let complexity = classify_task_complexity(task_description);
    let mut parts = Vec::new();

    parts.push("## Task Decomposition\n".to_string());

    match complexity {
        TaskComplexity::Atomic => {
            parts.push(format!(
                "This appears to be a **single-step task**. Implement directly without decomposition.\n\
                 \n\
                 If you discover the task is more complex than expected, you can still decompose — \
                 but start by attempting a direct implementation.\n\
                 \n\
                 You may still create new tasks for bugs, missing docs, or follow-up work you discover:\n\
                 - **Bug/issue found**: \
                 `wg add 'Fix: ...' --after {task_id} -d 'Found while working on {task_id}'`\n\
                 - **Follow-up needed**: `wg add 'Verify: ...' --after {task_id}`",
            ));
        }
        TaskComplexity::MultiStep => {
            parts.push(format!(
                "This appears to be a **multi-step task**. Consider decomposing with dependencies.\n\
                 \n\
                 ### DEFAULT: Attempt direct implementation first\n\
                 Even for multi-step tasks, start by implementing directly. Fanout is a tool, not a \
                 default — overkill fanout on manageable tasks wastes tokens and adds coordination friction.\n\
                 \n\
                 ### Fan out when:\n\
                 - 3+ independent files/components need changes that can genuinely run in parallel\n\
                 - You hit context pressure: re-reading files you already read, losing track of changes\n\
                 - The task has natural parallelism (e.g., 3 separate test files, N independent modules)\n\
                 - Your turn count exceeds 25 with no test progress\n\
                 \n\
                 ### Stay inline when:\n\
                 - The task is straightforward, even if it touches multiple files sequentially\n\
                 - Each step depends on the previous (sequential work doesn't parallelize)\n\
                 - Simple fixes, config changes, small features\n\
                 - The task is hard but single-scope — difficulty alone is NOT a reason to decompose\n\
                 \n\
                 ### If you decompose:\n\
                 - Each subtask MUST list its file scope in the description — **NO two subtasks may modify the same file**\n\
                 - Subtask descriptions should include \"Implement directly — do not decompose further\"\n\
                 - Always include a verify/integration task at the end: \
                 `wg add 'Integrate' --after part-a,part-b`\n\
                 - ALWAYS use `--after` to express dependencies. \
                 Flat task lists without dependency edges are an anti-pattern.\n\
                 - Log your decision: `wg log {task_id} \"FANOUT_DECISION: decompose — <reason>\"`\n\
                 \n\
                 ### Decomposition Templates\n\
                 Choose the pattern that best fits your task:"
            ));
            parts.push(DECOMP_TEMPLATE_PIPELINE.replace("{{task_id}}", task_id));
            parts.push(DECOMP_TEMPLATE_FAN_OUT.replace("{{task_id}}", task_id));
            parts.push(DECOMP_TEMPLATE_ITERATE.replace("{{task_id}}", task_id));
        }
    }

    // Common guidance for both classifications
    parts.push(format!(
        "\n### Include validation criteria in subtasks\n\
         Every code subtask description MUST include a `## Validation` section with concrete acceptance criteria. \
         Use `--validation llm` so an independent LLM evaluator checks the criteria, \
         or `--verify 'cargo test <name>'` when a specific shell check suffices:\n\
         ```bash\n\
         wg add 'Implement auth endpoint' --after {task_id} \\\n  \
         --validation llm \\\n  \
         -d '## Description\nAdd POST /auth/token endpoint.\n\n\
         ## Validation\n\
         - [ ] Failing test written first: test_auth_rejects_expired_token\n\
         - [ ] Implementation makes the test pass\n\
         - [ ] cargo test passes with no regressions'\n\
         ```",
    ));

    parts.push(format!(
        "\n### Guardrails\n\
         - You can create up to **{max_child_tasks}** subtasks per session (configurable via `wg config`)\n\
         - Task chains have a maximum depth of **{max_task_depth}** levels\n\
         - Always include an integrator at join points — don't leave parallel work unmerged",
    ));

    parts.push(
        "\n### When NOT to decompose\n\
         - The task is small and well-scoped (just do it)\n\
         - Decomposition overhead exceeds the work itself\n\
         - The subtasks would all modify the same files (serialize instead)"
            .to_string(),
    );

    parts.join("\n")
}

/// Git hygiene rules for agents working in a shared repository.
pub const GIT_HYGIENE_SECTION: &str = "\
## Git Hygiene (Shared Repo Rules)

You share a working tree with other agents. Follow these rules strictly:

- **Surgical staging only.** NEVER use `git add -A` or `git add .`. Always list specific files: `git add src/foo.rs src/bar.rs`
- **Verify before committing.** Run `git diff --cached --name-only` — every file must be one YOU modified for YOUR task. Unstage others' files with `git restore --staged <file>`.
- **Commit early, commit often.** Don't accumulate large uncommitted deltas. Commit after each logical unit of work.
- **NEVER stash.** Do not run `git stash`. If you see uncommitted changes from another agent, leave them alone.
- **NEVER force push.** No `git push --force`.
- **Don't touch others' changes.** If `git status` shows files you didn't modify, do not stage, commit, stash, or reset them.
- **Handle locks gracefully.** `.git/index.lock` or cargo target locks mean another agent is working. Wait 2-3 seconds and retry. Don't delete lock files.\n";

/// Worktree isolation warning for agents running in wg-managed worktrees.
/// Prevents agents from calling EnterWorktree/ExitWorktree which escapes the wg worktree.
pub const WORKTREE_ISOLATION_SECTION: &str = "\
## CRITICAL: Worktree Isolation

You are running inside a **workgraph-managed worktree**. Your working directory is already isolated.

**NEVER use the `EnterWorktree` or `ExitWorktree` tools.** Using them will:
1. Create a SECOND worktree in `.claude/worktrees/`, abandoning this one
2. Switch your session CWD away from the workgraph branch
3. Cause ALL your commits to go to the wrong branch
4. Result in your work being LOST — the merge-back will find no commits

If you see these tools available, **ignore them completely**. Workgraph already provides full git isolation.\n";

/// Message polling instructions for agents.
/// Contains {{task_id}} placeholder for variable substitution.
pub const MESSAGE_POLLING_SECTION: &str = "\
## Messages

Check for new messages periodically during long-running tasks:
```bash
wg msg read {{task_id}} --agent $WG_AGENT_ID
```
Messages may contain updated requirements, context from other agents,
or instructions from the user. Check at natural breakpoints in your work.

If there are messages, reply to each one:
```bash
wg msg send {{task_id}} \"Acknowledged — adjusting approach per your feedback\"
```\n";

/// Telegram escalation instructions for agents when Telegram is configured.
pub const TELEGRAM_ESCALATION_SECTION: &str = "\
## Human Escalation via Telegram

When you need human input, guidance, or approval, you can contact the user directly via Telegram:

### Send a Message
```bash
wg telegram send \"Your message here\"
```

### When to Escalate
**DO escalate for:**
- Blocking questions where you cannot proceed without clarification
- Ambiguous or contradictory requirements
- Permission needed for potentially destructive operations
- Critical decisions that could significantly impact the project

**DON'T escalate for:**
- Implementation details you can research or figure out
- Minor style/preference choices
- Standard development practices
- Routine errors you can debug and fix

### Conversation Protocol
1. **Send your message** clearly explaining what you need
2. **Continue with your task** if possible while waiting for a response
3. **Check for replies** periodically during your work (every 2-3 minutes for urgent matters)
4. **Wait up to 10 minutes** total for time-sensitive decisions
5. **Proceed with best judgment** if no response after 10 minutes, and log your decision:
   ```bash
   wg log {{task_id}} \"No response after 10min, proceeding with approach X - can be adjusted if needed\"
   ```

**Note**: The current implementation supports sending messages. Reply detection is planned for a future update.\n";

/// Hint for task+ scopes about using wg context/show to get more info (R2).
const WG_CONTEXT_HINT: &str = "\
## Additional Context
- Use `wg show <task-id>` to inspect any task's details, status, artifacts, and logs
- Use `wg context` to view the current task's full context
- Use `wg list` to see all tasks and their statuses\n";

/// Native executor tool guidance. Injected into the prompt only when the
/// executor is `native`, since these tool names are specific to the native
/// executor's in-process tool registry — claude/amplifier/etc. have
/// different names provided by their own runtimes.
///
/// The goal is to make the full native toolset visible in the system
/// prompt narrative, not just as API tool definitions. Models — especially
/// smaller ones with strong bash training priors — otherwise default to
/// shell-based workarounds for problems that have better first-class tools
/// (echo/heredoc for file creation, sed for editing, curl for web access,
/// polling loops for async work, etc.).
pub const NATIVE_FILE_TOOLS_SECTION: &str = "\
## Native Executor Tools

You have a rich in-process toolset. **Prefer the dedicated tool over a bash \
equivalent whenever one exists.** Bash is for things without a dedicated tool.

### File operations (no shell escaping, structured results)

- `read_file(path, offset?, limit?)` — read a file or a slice. Replaces `cat`/`head`/`tail`.
- `write_file(path, content)` — create or overwrite a file. Replaces `echo >` and \
heredocs. **Shell escaping of multi-line content is fragile — this is the #1 cause \
of failed file creation.** Never use bash for new-file creation.
- `edit_file(path, old_string, new_string)` — surgical in-place replacement. \
Replaces `sed -i`. `old_string` must appear exactly once; include surrounding \
context when needed to make it unique.
- `grep(pattern, path?, ...)` — search file contents. Replaces `grep -r`.
- `glob(pattern)` — find files by name pattern. Replaces `find` and shell globbing.

### Running programs

- `bash(command)` — **synchronous** shell execution. Use for tests, builds, \
system inspection, and quick one-shots. Outputs >2KB are channeled to disk \
automatically (see below).
- `bg(action, ...)` — **background/detached** execution for long-running commands \
(cargo build, test suites, servers) that would otherwise block your turn loop. \
Actions: `run`, `list`, `status`, `output`, `kill`, `delete`. Completion \
notifications inject into your next turn automatically. **Use `bg` — never \
`bash: nohup X &` or `while sleep; do check; done` — for anything longer than a \
few seconds.**

### Web access (structured content, no HTML parsing)

- `web_search(query, max_results?)` — search the web, get ranked title/URL/snippet \
results. Use instead of shelling out to curl+scraping.
- `web_fetch(url)` — fetch a page and get clean markdown (navigation/ads/scripts \
stripped, code blocks and tables preserved). Use instead of `bash: curl $URL` which \
gives you raw HTML.

### Delegation and summarization (push work out of your context)

- `delegate(prompt, exec_mode?, max_turns?)` — spawn a focused in-process sub-agent \
with its own conversation context. **The sub-agent's token usage does NOT count \
against your context** — only its final result text does. Use this for focused \
queries that would otherwise bloat your own context window: \"read src/X.rs and list \
its public functions\", \"find all callers of Y\", \"summarize the tests in tests/Z/\". \
`exec_mode=light` (default) gives read-only tools; `exec_mode=full` gives the full \
set minus `delegate` itself. `max_turns` caps the sub-agent at 5 (default) to 20 turns.

- `summarize(source, instruction?, max_input_bytes?)` — recursively summarize a \
**large text source** via map-reduce. Takes a file path OR inline text, chunks it \
to fit the model's context, summarizes each chunk independently with your \
instruction, then merges — recursing on the merged summaries if they're still \
too large. Use this when a source is too big to read directly: long log files, \
big text dumps, transcripts, large documents. Unlike `delegate`, `summarize` \
issues direct text-in/text-out LLM calls with no tool loop — cheap, predictable, \
and able to handle sources that would otherwise require many turns of manual \
chunking. Hard ceiling 1 MB by default (raisable via `max_input_bytes`).

### Workgraph task management (in-process, no CLI spawn)

- `wg_show(task_id)`, `wg_list()`, `wg_log(task_id, message)` — inspect and \
annotate tasks.
- `wg_add(title, description?, after?, tags?, skills?)` — create follow-up tasks \
(e.g., \"Verify: ...\" after your current task for fan-out).
- `wg_done(task_id)`, `wg_fail(task_id, reason)`, `wg_artifact(task_id, path)` — \
lifecycle operations.

Prefer these over `bash: wg show ...` — they take structured input and return \
structured results. For advanced flags not in the tool schemas \
(`--subtask` for blocking subtask, `--cron \"expr\"` for scheduled tasks), fall \
back to `bash: wg add --subtask ...` or `bash: wg add --cron ...`.

### Channeled tool outputs

When any tool returns more than ~2KB, the full output is saved to \
`.workgraph/agents/<agent-id>/tool-outputs/NNNNN.log` and replaced in your \
conversation with a compact handle plus a short preview. The raw bytes are always \
on disk — do NOT re-fetch from the original source. To read more from a channeled \
output, use either `read_file` with `offset`/`limit` on the handle path, or `bash` \
for text slicing (`sed -n 'A,Bp'`, `grep -n`, `wc -l`, `head`, `tail`).

### Anti-patterns — DO NOT do these

- `echo \"...\" > file` → use `write_file`
- `cat <<EOF > file ... EOF` → use `write_file`
- `sed -i 's/a/b/' file` → use `edit_file`
- `find . -name '*.rs'` → use `glob` with `**/*.rs`
- `grep -r foo src/` → use `grep`
- `curl $URL | lynx -dump` → use `web_fetch`
- `nohup long_command &` / `tmux new-session` → use `bg run`
- `while ! ls output; do sleep 5; done` → use `bg` with status checks
- Reading a whole huge file into context when you only need a slice → use \
`read_file(offset, limit)` or `delegate` to a sub-agent
";

/// Default workgraph usage guide for non-Claude models.
///
/// Injected into the prompt when the executor is non-Claude (native) so that models
/// without CLAUDE.md context understand wg basics. Users can override this by placing
/// a custom guide at `.workgraph/wg-guide.md`.
pub const DEFAULT_WG_GUIDE: &str = "\
**Workgraph (wg)** is a task coordination graph for AI agents. You are an agent \
working on one task in this graph. Other agents work on other tasks concurrently.

### Task Lifecycle
Tasks move through: `open` → `in-progress` → `done` / `failed` / `abandoned`.
Tasks with `--validation llm`, `--verify`, or `--validation external` enter `pending-validation` before `done`.

### Core Commands

| Command | Purpose |
|---------|---------|
| `wg show <id>` | View task details, status, deps, logs |
| `wg log <id> \"msg\"` | Log progress (recoverable breadcrumbs) |
| `wg artifact <id> path` | Record a file you created/modified |
| `wg done <id>` | Mark your task complete |
| `wg fail <id> --reason \"...\"` | Mark your task failed |
| `wg add \"title\" -d \"desc\"` | Create a new task |
| `wg list` | List all tasks |
| `wg ready` | List tasks ready to be worked on |
| `wg msg read <id>` | Check for messages on your task |
| `wg msg send <id> \"msg\"` | Send a message on your task |

### Dependencies with `--after`
Use `--after` to express that one task depends on another. This is CRITICAL \
for correct execution order.

```bash
# Task B depends on Task A completing first
wg add \"Task B\" --after task-a

# Task C depends on multiple predecessors
wg add \"Task C\" --after task-a,task-b

# Subtask that depends on current task
wg add \"Subtask\" --after $CURRENT_TASK_ID
```

**Always use `--after` when creating subtasks.** Without it, tasks form a flat \
unordered list and may execute in the wrong order.

### Verification (pending-validation gate)
Three modes add a gate before a task completes:

- `--validation llm` **(preferred)** — LLM evaluator auto-approves or rejects based on the `## Validation` section
- `--verify \"cargo test test_name\"` — shell check gate; use only for a cheap, specific test
- `--validation external` — human must manually `wg approve` before the task closes

```bash
wg add \"Implement feature\" --validation llm -d \"## Validation\\n- [ ] test_feature passes\"
```

When `wg done` is called, the task moves to `pending-validation`. The coordinator dispatches \
the evaluator (for `llm` mode), which calls `wg approve` or `wg reject` automatically.

### When to Decompose vs Implement Directly
- **Implement directly** if the task is small, well-scoped, and touches ≤ 2-3 files
- **Decompose** (create subtasks with `wg add --after`) if:
  - The task has 3+ independent parts
  - You discover bugs or missing prereqs outside your scope
  - The work would take multiple distinct phases

### Environment Variables
- `$WG_AGENT_ID` — your unique agent identifier
- `$WG_TASK_ID` — the task you are working on
- `$WG_EXECUTOR_TYPE` — executor type (native, claude, etc.)
- `$WG_MODEL` — the model you are running as\n";

/// Pattern keyword trigger words. If any of these appear (case-insensitive, word-boundary)
/// in the task description, the glossary section is included in the prompt.
const PATTERN_TRIGGER_KEYWORDS: &[&str] = &[
    "autopoietic",
    "self-organizing",
    "committee",
    "discussion",
    "deliberation",
    "swarm",
    "fork-join",
    "fan-out",
    "parallel",
    "loop",
    "cycle",
    "iterate",
    "research",
    "investigate",
    "audit",
];

/// Pattern keyword glossary injected when the task description contains trigger keywords.
/// Teaches agents the expected behavior for each organizational pattern.
pub const PATTERN_KEYWORDS_GLOSSARY: &str = "\
## Pattern Keywords

Your task description uses organizational pattern vocabulary. Here is what each pattern expects:

- **autopoietic / self-organizing**: Decompose this work into subtasks using `wg add`. Create your own task graph with proper dependencies. Don't try to do everything yourself — break it into pieces and let the coordinator dispatch them.

- **committee / discussion / deliberation / swarm**: Spawn multiple parallel tasks (via `wg add`) representing different perspectives or approaches. Each task produces a position/analysis. Create a synthesis task (`--after` all perspectives) that integrates findings. Use `wg msg` to communicate between tasks if needed.

- **fork-join / fan-out / parallel**: Create N parallel subtasks for independent work, plus one integration task that depends on all of them (`--after task1,task2,...,taskN`).

- **loop / cycle / iterate**: Use `--max-iterations` on tasks. Each iteration should build on the previous. Use `wg done --converged` when the work has stabilized. If verify fails and you can't fix it, use `wg fail` so the cycle can restart.

- **research / investigate / audit**: Produce a structured document with findings. Reference specific files and line numbers. Create implementation subtasks if the research reveals work to be done.

For detailed pattern descriptions, see docs/research/organizational-patterns.md\n";

/// Check whether a task description contains any pattern trigger keywords.
/// Uses case-insensitive matching.
pub fn description_has_pattern_keywords(description: &str) -> bool {
    let lower = description.to_lowercase();
    PATTERN_TRIGGER_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// Triage mode prompt section: injected when any dependency has status=Failed.
/// Instructs the agent to create fix tasks and requeue instead of proceeding.
pub const TRIAGE_MODE_SECTION: &str = "\
## Failed Dependency Protocol

Before starting your own work, check your dependency context.
If ANY dependency has status=Failed:

1. **DO NOT** proceed with your own task work.
2. Read the failure reason and logs from the failed dependency.
3. Assess whether you can create fix tasks:
   a. If the failure is clear and scoped → create fix task(s) via `wg add`
   b. If the failure is ambiguous or cascading → create a research/investigate task
   c. If you cannot determine a fix after investigating → `wg fail` with what you investigated and what specifically blocks progress
4. Create fix tasks that block the failed dep (so it re-runs after the fix):
   ```
   wg add \"Fix: <description>\" --before <failed-dep-id> \\
     --verify \"<validation command>\" \\
     -d \"<details from failure logs>\"
   ```
5. Retry the failed dependency:
   ```
   wg retry <failed-dep-id>
   ```
6. Requeue yourself:
   ```
   wg requeue {{task_id}} --reason \"Created fix tasks for failed dep <dep-id>\"
   ```
7. **Exit immediately** (do not do any other work).\n";

/// Additional context for scope-based prompt assembly beyond TemplateVars.
#[derive(Debug, Default, Clone)]
pub struct ScopeContext {
    /// Downstream task IDs + titles (R1, task+ scope)
    pub downstream_info: String,
    /// Task tags and skills (R4, task+ scope)
    pub tags_skills_info: String,
    /// Project description from config.toml (graph+ scope)
    pub project_description: String,
    /// 1-hop subgraph summary (graph+ scope)
    pub graph_summary: String,
    /// Full graph summary (full scope)
    pub full_graph_summary: String,
    /// CLAUDE.md content (full scope)
    pub claude_md_content: String,
    /// Queued messages for this task (task+ scope)
    pub queued_messages: String,
    /// Context from a previous agent attempt (injected on retry)
    pub previous_attempt_context: String,
    /// Workgraph usage guide for non-Claude models (injected when model lacks CLAUDE.md)
    pub wg_guide_content: String,
    /// Discovered test files formatted for prompt injection (task+ scope)
    pub discovered_tests: String,
    /// Whether adaptive decomposition guidance is enabled (from config)
    pub decomp_guidance: bool,
    /// Whether Telegram escalation is configured and available (task+ scope)
    pub telegram_available: bool,
    /// Whether to inject the native-executor file-tool guidance section.
    /// Set when the spawning executor is `native` so the model learns it
    /// has `read_file`/`write_file`/`edit_file`/`grep`/`glob` available
    /// and should prefer them over bash equivalents.
    pub native_file_tools: bool,
}

/// Build a scope-aware prompt for built-in executors.
///
/// Assembles prompt sections based on the context scope:
/// - `clean`: skills_preamble + identity + task info + upstream context + loop info only
/// - `task`: + workflow commands + graph patterns + reusable functions + wg cli warning + R1/R2/R4
/// - `graph`: + project description + subgraph summary (1-hop neighborhood)
/// - `full`: + system awareness preamble + full graph summary + CLAUDE.md content
pub fn build_prompt(vars: &TemplateVars, scope: ContextScope, ctx: &ScopeContext) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Full scope: system awareness preamble (at the top for orientation)
    if scope >= ContextScope::Full {
        parts.push(SYSTEM_AWARENESS_PREAMBLE.to_string());
    }

    // All scopes: skills preamble
    if !vars.skills_preamble.is_empty() {
        parts.push(vars.skills_preamble.clone());
    }

    // All scopes: task assignment header
    parts.push(
        "# Task Assignment\n\nYou are an AI agent working on a task in a workgraph project.\n"
            .to_string(),
    );

    // All scopes: agent identity
    if !vars.task_identity.is_empty() {
        parts.push(vars.task_identity.clone());
    }

    // All scopes: task details
    parts.push(format!(
        "## Your Task\n- **ID:** {}\n- **Title:** {}\n- **Description:** {}",
        vars.task_id, vars.task_title, vars.task_description
    ));

    // All scopes: pattern keywords glossary (conditional on description content)
    if description_has_pattern_keywords(&vars.task_description) {
        parts.push(PATTERN_KEYWORDS_GLOSSARY.to_string());
    }

    // All scopes: verification criteria (R4 from validation synthesis)
    if let Some(ref verify) = vars.task_verify {
        parts.push(format!(
            "## Verification Required\n\nBefore marking done, you MUST verify:\n{}",
            verify
        ));
    }

    // Task+ scope: discovered test files
    if scope >= ContextScope::Task && !ctx.discovered_tests.is_empty() {
        parts.push(ctx.discovered_tests.clone());
    }

    // Task+ scope: tags and skills (R4)
    if scope >= ContextScope::Task && !ctx.tags_skills_info.is_empty() {
        parts.push(ctx.tags_skills_info.clone());
    }

    // All scopes: context from dependencies
    parts.push(format!(
        "## Context from Dependencies\n{}",
        vars.task_context
    ));

    // All scopes: triage mode (injected when any dependency is Failed)
    if vars.has_failed_deps {
        let mut triage_section = format!(
            "## Failed Dependencies Detected — TRIAGE MODE\n\n\
             The following dependencies have FAILED:\n{}",
            vars.failed_deps_info
        );
        triage_section.push_str("\n\nYou are in TRIAGE mode. Do NOT proceed with your normal task work.\nFollow the Failed Dependency Protocol below.\n");
        parts.push(triage_section);
        parts.push(vars.apply(TRIAGE_MODE_SECTION));
    }

    // All scopes: previous attempt context (injected on retry)
    if !ctx.previous_attempt_context.is_empty() {
        parts.push(ctx.previous_attempt_context.clone());
    }
    // Task+ scope: queued messages
    if scope >= ContextScope::Task && !ctx.queued_messages.is_empty() {
        parts.push(ctx.queued_messages.clone());
    }

    // Task+ scope: downstream awareness (R1)
    if scope >= ContextScope::Task && !ctx.downstream_info.is_empty() {
        parts.push(ctx.downstream_info.clone());
    }

    // All scopes: loop info
    if !vars.task_loop_info.is_empty() {
        parts.push(vars.task_loop_info.clone());
    }

    // Task+ scope: wg usage guide for non-Claude models
    if scope >= ContextScope::Task && !ctx.wg_guide_content.is_empty() {
        parts.push(format!(
            "## Workgraph Usage Guide\n\n{}",
            ctx.wg_guide_content
        ));
    }

    // Task+ scope: native-executor file-tool guidance. Teaches the model
    // that it has dedicated read_file/write_file/edit_file/grep/glob tools
    // and should prefer them over bash equivalents (echo/cat/heredoc/sed).
    // This is foundational — place it near the top of the guidance stack.
    if scope >= ContextScope::Task && ctx.native_file_tools {
        parts.push(NATIVE_FILE_TOOLS_SECTION.to_string());
    }

    // Task+ scope: workflow sections (with {{task_id}} substitution)
    if scope >= ContextScope::Task {
        parts.push(vars.apply(REQUIRED_WORKFLOW_SECTION));
        parts.push(GIT_HYGIENE_SECTION.to_string());
        if vars.in_worktree {
            parts.push(WORKTREE_ISOLATION_SECTION.to_string());
        }
        parts.push(vars.apply(MESSAGE_POLLING_SECTION));

        // Task+ scope: Telegram escalation (when configured)
        if ctx.telegram_available {
            parts.push(vars.apply(TELEGRAM_ESCALATION_SECTION));
        }

        parts.push(vars.apply(ETHOS_SECTION));
        if ctx.decomp_guidance {
            parts.push(build_decomposition_guidance(
                &vars.task_description,
                &vars.task_id,
                vars.max_child_tasks,
                vars.max_task_depth,
            ));
        } else {
            parts.push(vars.apply(AUTOPOIETIC_GUIDANCE));
        }
        parts.push(RESEARCH_HINTS_SECTION.to_string());
        parts.push(GRAPH_PATTERNS_SECTION.to_string());
        parts.push(REUSABLE_FUNCTIONS_SECTION.to_string());
        parts.push(vars.apply(CRITICAL_WG_CLI_SECTION));
        parts.push(WG_CONTEXT_HINT.to_string());
    }

    // Graph+ scope: project description
    if scope >= ContextScope::Graph && !ctx.project_description.is_empty() {
        parts.push(format!("## Project\n\n{}", ctx.project_description));
    }

    // Graph+ scope: subgraph summary (1-hop neighborhood)
    if scope >= ContextScope::Graph && !ctx.graph_summary.is_empty() {
        parts.push(ctx.graph_summary.clone());
    }

    // Full scope: full graph summary
    if scope >= ContextScope::Full && !ctx.full_graph_summary.is_empty() {
        parts.push(ctx.full_graph_summary.clone());
    }

    // Full scope: CLAUDE.md content
    if scope >= ContextScope::Full && !ctx.claude_md_content.is_empty() {
        parts.push(format!(
            "## Project Instructions (CLAUDE.md)\n\n{}",
            ctx.claude_md_content
        ));
    }

    parts.push("Begin working on the task now.".to_string());

    let assembled_prompt = parts.join("\n\n");

    // Debug logging: capture complete prompt if WG_DEBUG_PROMPTS is set
    if std::env::var("WG_DEBUG_PROMPTS").is_ok()
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/wg_debug_prompts.log")
    {
        use std::io::Write;
        let debug_info = format!(
            "=== WG DEBUG: Assembled Prompt for Task {} ===\n\
            Scope: {:?}\n\
            Model: {}\n\
            Prompt length: {} characters\n\
            Prompt content:\n\
            {}\n\
            === End of Prompt ===\n\n",
            vars.task_id,
            scope,
            vars.model,
            assembled_prompt.len(),
            assembled_prompt
        );
        let _ = file.write_all(debug_info.as_bytes());
    }

    assembled_prompt
}

/// Template variables that can be used in executor configurations.
#[derive(Debug, Clone)]
pub struct TemplateVars {
    pub task_id: String,
    pub task_title: String,
    pub task_description: String,
    pub task_context: String,
    pub task_identity: String,
    pub working_dir: String,
    pub skills_preamble: String,
    pub model: String,
    pub task_loop_info: String,
    pub task_verify: Option<String>,
    pub max_child_tasks: u32,
    pub max_task_depth: u32,
    /// True when any dependency of the task has status=Failed (triggers triage mode)
    pub has_failed_deps: bool,
    /// Info about failed dependencies for triage prompt injection
    pub failed_deps_info: String,
    /// True when the agent is running in a wg-managed worktree
    pub in_worktree: bool,
}

impl TemplateVars {
    /// Create template variables from a task, optional context, and optional workgraph directory.
    ///
    /// If the task has an agent set and `workgraph_dir` is provided, the Agent is loaded
    /// by hash and its role and motivation are resolved from agency storage and rendered
    /// into an identity prompt. If resolution fails or no agent is set, `task_identity`
    /// is empty (backward compatible).
    pub fn from_task(task: &Task, context: Option<&str>, workgraph_dir: Option<&Path>) -> Self {
        let task_identity = Self::resolve_identity(task, workgraph_dir);

        let working_dir = workgraph_dir
            .and_then(|d| {
                // Canonicalize to resolve relative paths like ".workgraph"
                // whose parent() would be "" instead of the actual directory.
                let abs = d.canonicalize().ok()?;
                abs.parent().map(|p| p.to_string_lossy().to_string())
            })
            .unwrap_or_default();

        let skills_preamble = Self::resolve_skills_preamble(workgraph_dir);

        let task_loop_info = if let Some(config) = &task.cycle_config {
            format!(
                "## Cycle Information\n\n\
                 This task is a cycle header (iteration {}/{}).\n\n\
                 **IMPORTANT: When this cycle's work is complete (converged), you MUST use:**\n\
                 ```\n\
                 wg done {} --converged\n\
                 ```\n\
                 Using plain `wg done` will cause the cycle to iterate again and re-open tasks.\n\
                 Only use plain `wg done` if you want the next iteration to proceed.",
                task.loop_iteration + 1,
                config.max_iterations,
                task.id
            )
        } else if task.loop_iteration > 0 {
            format!(
                "## Cycle Information\n\n\
                 This task is in cycle iteration {}.\n\n\
                 **IMPORTANT: When this cycle's work is complete (converged), you MUST use:**\n\
                 ```\n\
                 wg done {} --converged\n\
                 ```",
                task.loop_iteration + 1,
                task.id
            )
        } else {
            String::new()
        };

        // Load guardrails config for autopoietic limits
        let guardrails = workgraph_dir
            .and_then(|dir| crate::config::Config::load_merged(dir).ok())
            .map(|cfg| cfg.guardrails)
            .unwrap_or_default();

        Self {
            task_id: task.id.clone(),
            task_title: task.title.clone(),
            task_description: task.description.clone().unwrap_or_default(),
            task_context: context.unwrap_or_default().to_string(),
            task_identity,
            working_dir,
            skills_preamble,
            model: task.model.clone().unwrap_or_default(),
            task_loop_info,
            task_verify: task.verify.clone(),
            max_child_tasks: guardrails.max_child_tasks_per_agent,
            max_task_depth: guardrails.max_task_depth,
            has_failed_deps: false,
            failed_deps_info: String::new(),
            in_worktree: false,
        }
    }

    /// Resolve the identity prompt for a task by looking up its Agent, then the
    /// Agent's role and motivation.
    fn resolve_identity(task: &Task, workgraph_dir: Option<&Path>) -> String {
        let agent_hash = match &task.agent {
            Some(h) => h,
            None => return String::new(),
        };

        let wg_dir = match workgraph_dir {
            Some(dir) => dir,
            None => return String::new(),
        };

        let agency_dir = wg_dir.join("agency");
        let agents_dir = agency_dir.join("cache/agents");
        let roles_dir = agency_dir.join("cache/roles");
        let motivations_dir = agency_dir.join("primitives/tradeoffs");

        // Look up the Agent entity by hash
        let agent = match agency::find_agent_by_prefix(&agents_dir, agent_hash) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("Warning: could not resolve agent '{}': {}", agent_hash, e);
                return String::new();
            }
        };

        let role = match agency::find_role_by_prefix(&roles_dir, &agent.role_id) {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "Warning: could not resolve role '{}' for agent '{}': {}",
                    agent.role_id, agent_hash, e
                );
                return String::new();
            }
        };

        let motivation = match agency::find_tradeoff_by_prefix(&motivations_dir, &agent.tradeoff_id)
        {
            Ok(m) => m,
            Err(e) => {
                eprintln!(
                    "Warning: could not resolve motivation '{}' for agent '{}': {}",
                    agent.tradeoff_id, agent_hash, e
                );
                return String::new();
            }
        };

        // Resolve skills from the role, using the project root (parent of .workgraph/)
        let workgraph_root = wg_dir.parent().unwrap_or(wg_dir);
        let resolved_skills = agency::resolve_all_components(&role, workgraph_root, &agency_dir);
        let outcome = agency::resolve_outcome(&role.outcome_id, &agency_dir);

        agency::render_identity_prompt_rich(&role, &motivation, &resolved_skills, outcome.as_ref())
    }

    /// Read skills preamble from project-level `.claude/skills/` directory.
    ///
    /// If `using-superpowers/SKILL.md` exists, its content is included so that
    /// agents spawned via `--print` (which don't trigger SessionStart hooks)
    /// still get the skill-invocation discipline.
    fn resolve_skills_preamble(workgraph_dir: Option<&Path>) -> String {
        let project_root = match workgraph_dir.and_then(|d| {
            // Canonicalize to handle relative paths like ".workgraph"
            d.canonicalize()
                .ok()
                .and_then(|abs| abs.parent().map(std::path::Path::to_path_buf))
        }) {
            Some(r) => r,
            None => return String::new(),
        };

        let skill_path = project_root
            .join(".claude")
            .join("skills")
            .join("using-superpowers")
            .join("SKILL.md");

        match std::fs::read_to_string(&skill_path) {
            Ok(content) => {
                // Strip YAML frontmatter if present
                let body = if content.starts_with("---") {
                    // splitn(3, "---") on "---\nfoo: bar\n---\nbody" gives ["", "\nfoo: bar\n", "\nbody"]
                    // If there's no closing ---, nth(2) is None; skip past the first line instead.
                    content
                        .splitn(3, "---")
                        .nth(2)
                        .unwrap_or_else(|| {
                            // Malformed frontmatter (no closing ---): skip the opening --- line
                            content
                                .strip_prefix("---")
                                .and_then(|s| s.split_once('\n').map(|(_, rest)| rest))
                                .unwrap_or("")
                        })
                        .trim()
                } else {
                    content.trim()
                };
                format!(
                    "<EXTREMELY_IMPORTANT>\nYou have superpowers.\n\n\
                     Below is your introduction to using skills. \
                     For all other skills, use the Skill tool:\n\n\
                     {}\n</EXTREMELY_IMPORTANT>\n",
                    body
                )
            }
            Err(_) => String::new(),
        }
    }

    /// Apply template substitution to a string.
    pub fn apply(&self, template: &str) -> String {
        template
            .replace("{{task_id}}", &self.task_id)
            .replace("{{task_title}}", &self.task_title)
            .replace("{{task_description}}", &self.task_description)
            .replace("{{task_context}}", &self.task_context)
            .replace("{{task_identity}}", &self.task_identity)
            .replace("{{working_dir}}", &self.working_dir)
            .replace("{{skills_preamble}}", &self.skills_preamble)
            .replace("{{model}}", &self.model)
            .replace("{{task_loop_info}}", &self.task_loop_info)
            .replace("{{task_verify}}", self.task_verify.as_deref().unwrap_or(""))
            .replace("{{max_child_tasks}}", &self.max_child_tasks.to_string())
            .replace("{{max_task_depth}}", &self.max_task_depth.to_string())
    }
}

/// Configuration for an executor, loaded from `.workgraph/executors/<name>.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutorConfig {
    /// The executor configuration section.
    pub executor: ExecutorSettings,
}

/// Settings within an executor configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutorSettings {
    /// Type of executor: "claude", "shell", "custom".
    #[serde(rename = "type")]
    pub executor_type: String,

    /// Command to execute.
    pub command: String,

    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment variables to set.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Prompt template configuration (optional).
    #[serde(default)]
    pub prompt_template: Option<PromptTemplate>,

    /// Working directory for the executor (optional).
    #[serde(default)]
    pub working_dir: Option<String>,

    /// Timeout in seconds (optional).
    #[serde(default)]
    pub timeout: Option<u64>,

    /// Default model for this executor.
    /// Overrides coordinator.model but is overridden by task.model.
    /// Hierarchy: task.model > executor.model > coordinator.model > 'default'.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Prompt template for injecting task context.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PromptTemplate {
    /// The template string with placeholders.
    #[serde(default)]
    pub template: String,
}

impl ExecutorConfig {
    /// Load executor configuration from a TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read executor config: {}", path.display()))?;

        let config: ExecutorConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse executor config: {}", path.display()))?;

        Ok(config)
    }

    /// Load executor configuration from the workgraph executors directory.
    pub fn load_by_name(workgraph_dir: &Path, name: &str) -> Result<Self> {
        let config_path = workgraph_dir
            .join("executors")
            .join(format!("{}.toml", name));

        if !config_path.exists() {
            return Err(anyhow!(
                "Executor config not found: {}. Create it at {}",
                name,
                config_path.display()
            ));
        }

        Self::load(&config_path)
    }

    /// Apply template variables to all configurable fields.
    pub fn apply_templates(&self, vars: &TemplateVars) -> ExecutorSettings {
        let mut settings = self.executor.clone();

        // Apply to command
        settings.command = vars.apply(&settings.command);

        // Apply to args
        settings.args = settings.args.iter().map(|arg| vars.apply(arg)).collect();

        // Apply to env vars
        settings.env = settings
            .env
            .iter()
            .map(|(k, v)| (k.clone(), vars.apply(v)))
            .collect();

        // Apply to prompt template
        if let Some(ref mut pt) = settings.prompt_template {
            pt.template = vars.apply(&pt.template);
        }

        // Apply to working dir
        if let Some(ref wd) = settings.working_dir {
            settings.working_dir = Some(vars.apply(wd));
        }

        settings
    }
}

/// Registry for loading executor configurations.
pub struct ExecutorRegistry {
    config_dir: PathBuf,
}

impl ExecutorRegistry {
    /// Create a new executor registry.
    pub fn new(workgraph_dir: &Path) -> Self {
        Self {
            config_dir: workgraph_dir.join("executors"),
        }
    }

    /// Load executor config by name.
    pub fn load_config(&self, name: &str) -> Result<ExecutorConfig> {
        let config_path = self.config_dir.join(format!("{}.toml", name));

        if config_path.exists() {
            ExecutorConfig::load(&config_path)
        } else {
            // Return a default config for built-in executors
            self.default_config(name)
        }
    }

    /// Get default config for built-in executors.
    fn default_config(&self, name: &str) -> Result<ExecutorConfig> {
        match name {
            "claude" => Ok(ExecutorConfig {
                executor: ExecutorSettings {
                    executor_type: "claude".to_string(),
                    command: "claude".to_string(),
                    args: vec![
                        "--print".to_string(),
                        "--verbose".to_string(),
                        "--permission-mode".to_string(),
                        "bypassPermissions".to_string(),
                        "--output-format".to_string(),
                        "stream-json".to_string(),
                    ],
                    env: HashMap::new(),
                    // No default template — built-in executors use scope-based
                    // build_prompt() assembly. Custom configs in
                    // .workgraph/executors/*.toml can still define a template
                    // to override this behavior.
                    prompt_template: None,
                    working_dir: Some("{{working_dir}}".to_string()),
                    timeout: None,
                    model: None,
                },
            }),
            "codex" => Ok(ExecutorConfig {
                executor: ExecutorSettings {
                    executor_type: "codex".to_string(),
                    command: "codex".to_string(),
                    args: vec![
                        "exec".to_string(),
                        "--json".to_string(),
                        "--skip-git-repo-check".to_string(),
                        "--dangerously-bypass-approvals-and-sandbox".to_string(),
                    ],
                    env: HashMap::new(),
                    // No default template — uses scope-based build_prompt() assembly.
                    prompt_template: None,
                    working_dir: Some("{{working_dir}}".to_string()),
                    timeout: None,
                    model: None,
                },
            }),
            "shell" => Ok(ExecutorConfig {
                executor: ExecutorSettings {
                    executor_type: "shell".to_string(),
                    command: "bash".to_string(),
                    args: vec!["-c".to_string(), "{{task_context}}".to_string()],
                    env: {
                        let mut env = HashMap::new();
                        env.insert("TASK_ID".to_string(), "{{task_id}}".to_string());
                        env.insert("TASK_TITLE".to_string(), "{{task_title}}".to_string());
                        env
                    },
                    prompt_template: None,
                    working_dir: None,
                    timeout: None,
                    model: None,
                },
            }),
            "amplifier" => Ok(ExecutorConfig {
                executor: ExecutorSettings {
                    executor_type: "amplifier".to_string(),
                    command: "amplifier".to_string(),
                    args: vec![
                        "run".to_string(),
                        "--mode".to_string(),
                        "single".to_string(),
                        "--output-format".to_string(),
                        "text".to_string(),
                    ],
                    env: {
                        let mut env = HashMap::new();
                        env.insert("WG_TASK_ID".to_string(), "{{task_id}}".to_string());
                        env
                    },
                    // No default template — uses scope-based build_prompt() assembly.
                    prompt_template: None,
                    working_dir: Some("{{working_dir}}".to_string()),
                    timeout: Some(600),
                    model: None,
                },
            }),
            "native" => Ok(ExecutorConfig {
                executor: ExecutorSettings {
                    executor_type: "native".to_string(),
                    command: "wg".to_string(),
                    args: vec!["native-exec".to_string()],
                    env: {
                        let mut env = HashMap::new();
                        env.insert("WG_TASK_ID".to_string(), "{{task_id}}".to_string());
                        env
                    },
                    // No default template — uses scope-based build_prompt() assembly.
                    prompt_template: None,
                    working_dir: Some("{{working_dir}}".to_string()),
                    timeout: None,
                    model: None,
                },
            }),
            "default" => Ok(ExecutorConfig {
                executor: ExecutorSettings {
                    executor_type: "default".to_string(),
                    command: "echo".to_string(),
                    args: vec!["Task: {{task_id}}".to_string()],
                    env: HashMap::new(),
                    prompt_template: None,
                    working_dir: None,
                    timeout: None,
                    model: None,
                },
            }),
            _ => Err(anyhow!(
                "Unknown executor '{}'. Available: claude, codex, amplifier, native, shell, default",
                name,
            )),
        }
    }

    /// Ensure the executors directory exists and has default configs.
    #[cfg(test)]
    pub fn init(&self) -> Result<()> {
        if !self.config_dir.exists() {
            fs::create_dir_all(&self.config_dir).with_context(|| {
                format!(
                    "Failed to create executors directory: {}",
                    self.config_dir.display()
                )
            })?;
        }

        // Create default executor configs if they don't exist
        for name in ["claude", "codex", "shell"] {
            let config_path = self.config_dir.join(format!("{}.toml", name));
            if !config_path.exists() {
                let config = self.default_config(name)?;
                let content = toml::to_string_pretty(&config)
                    .with_context(|| format!("Failed to serialize {} config", name))?;
                fs::write(&config_path, content)
                    .with_context(|| format!("Failed to write {} config", name))?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_test_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: Some("Test description".to_string()),
            status: crate::graph::Status::Open,
            priority: crate::graph::Priority::default(),
            assigned: None,
            estimate: None,
            before: vec![],
            after: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            timeout: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: None,
            provider: None,
            endpoint: None,
            verify: None,
            verify_timeout: None,
            agent: None,
            loop_iteration: 0,
            last_iteration_completed_at: None,
            cycle_failure_restarts: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
            context_scope: None,
            exec_mode: None,
            cycle_config: None,
            token_usage: None,
            session_id: None,
            wait_condition: None,
            checkpoint: None,
            triage_count: 0,
            resurrection_count: 0,
            last_resurrected_at: None,
            validation: None,
            validation_commands: vec![],
            validator_agent: None,
            validator_model: None,
            gate_attempts: 0,
            test_required: false,
            rejection_count: 0,
            max_rejections: None,
            verify_failures: 0,
            spawn_failures: 0,
            tried_models: vec![],
            superseded_by: vec![],
            supersedes: None,
            unplaced: false,
            place_before: vec![],
            place_near: vec![],
            independent: false,
            iteration_round: 0,
            iteration_anchor: None,
            iteration_parent: None,
            iteration_config: None,
            cron_schedule: None,
            cron_enabled: false,
            last_cron_fire: None,
            next_cron_fire: None,
        }
    }

    #[test]
    fn test_template_vars_apply() {
        let task = make_test_task("task-123", "Implement feature");
        let vars = TemplateVars::from_task(&task, Some("Context from deps"), None);

        let template = "Working on {{task_id}}: {{task_title}}. Context: {{task_context}}";
        let result = vars.apply(template);

        assert_eq!(
            result,
            "Working on task-123: Implement feature. Context: Context from deps"
        );
    }

    #[test]
    fn test_template_vars_from_task() {
        let task = make_test_task("my-task", "My Title");
        let vars = TemplateVars::from_task(&task, None, None);

        assert_eq!(vars.task_id, "my-task");
        assert_eq!(vars.task_title, "My Title");
        assert_eq!(vars.task_description, "Test description");
        assert_eq!(vars.task_context, "");
    }

    #[test]
    fn test_executor_config_load() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("test.toml");

        let config_content = r#"
[executor]
type = "custom"
command = "my-agent"
args = ["--task", "{{task_id}}"]

[executor.env]
TASK_TITLE = "{{task_title}}"

[executor.prompt_template]
template = "Work on {{task_id}}"
"#;
        fs::write(&config_path, config_content).unwrap();

        let config = ExecutorConfig::load(&config_path).unwrap();
        assert_eq!(config.executor.executor_type, "custom");
        assert_eq!(config.executor.command, "my-agent");
        assert_eq!(config.executor.args, vec!["--task", "{{task_id}}"]);
    }

    #[test]
    fn test_executor_config_apply_templates() {
        let config = ExecutorConfig {
            executor: ExecutorSettings {
                executor_type: "test".to_string(),
                command: "run-{{task_id}}".to_string(),
                args: vec!["--title".to_string(), "{{task_title}}".to_string()],
                env: {
                    let mut env = HashMap::new();
                    env.insert("TASK".to_string(), "{{task_id}}".to_string());
                    env
                },
                prompt_template: Some(PromptTemplate {
                    template: "Context: {{task_context}}".to_string(),
                }),
                working_dir: Some("/work/{{task_id}}".to_string()),
                timeout: None,
                model: None,
            },
        };

        let task = make_test_task("t-1", "Test Task");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let settings = config.apply_templates(&vars);

        assert_eq!(settings.command, "run-t-1");
        assert_eq!(settings.args, vec!["--title", "Test Task"]);
        assert_eq!(settings.env.get("TASK"), Some(&"t-1".to_string()));
        assert_eq!(
            settings.prompt_template.unwrap().template,
            "Context: dep context"
        );
        assert_eq!(settings.working_dir, Some("/work/t-1".to_string()));
    }

    #[test]
    fn test_executor_registry_default_configs() {
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());

        // Should return default configs for built-in executors
        let claude_config = registry.load_config("claude").unwrap();
        assert_eq!(claude_config.executor.executor_type, "claude");
        assert_eq!(claude_config.executor.command, "claude");

        let codex_config = registry.load_config("codex").unwrap();
        assert_eq!(codex_config.executor.executor_type, "codex");
        assert_eq!(codex_config.executor.command, "codex");

        let shell_config = registry.load_config("shell").unwrap();
        assert_eq!(shell_config.executor.executor_type, "shell");
        assert_eq!(shell_config.executor.command, "bash");
    }

    #[test]
    fn test_executor_registry_init() {
        let temp_dir = TempDir::new().unwrap();
        let workgraph_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(&workgraph_dir).unwrap();

        let registry = ExecutorRegistry::new(&workgraph_dir);
        registry.init().unwrap();

        // Should create executor configs
        assert!(workgraph_dir.join("executors/claude.toml").exists());
        assert!(workgraph_dir.join("executors/codex.toml").exists());
        assert!(workgraph_dir.join("executors/shell.toml").exists());
    }

    #[test]
    fn test_template_vars_no_identity_when_none() {
        let task = make_test_task("task-1", "Test Task");
        let vars = TemplateVars::from_task(&task, None, None);
        assert_eq!(vars.task_identity, "");
    }

    #[test]
    fn test_template_vars_no_identity_when_no_workgraph_dir() {
        let mut task = make_test_task("task-1", "Test Task");
        task.agent = Some("some-agent-hash".to_string());
        // No workgraph_dir provided, so identity should be empty
        let vars = TemplateVars::from_task(&task, None, None);
        assert_eq!(vars.task_identity, "");
    }

    #[test]
    fn test_template_vars_identity_resolved_from_agency() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        let roles_dir = wg_dir.join("agency").join("cache/roles");
        let motivations_dir = wg_dir.join("agency").join("primitives/tradeoffs");
        let agents_dir = wg_dir.join("agency").join("cache/agents");
        fs::create_dir_all(&roles_dir).unwrap();
        fs::create_dir_all(&motivations_dir).unwrap();
        fs::create_dir_all(&agents_dir).unwrap();

        // Create a role using content-hash ID builder
        let role = agency::build_role("Implementer", "Implements features", vec![], "Working code");
        let role_id = role.id.clone();
        agency::save_role(&role, &roles_dir).unwrap();

        // Create a motivation using content-hash ID builder
        let motivation = agency::build_tradeoff(
            "Quality First",
            "Prioritize quality",
            vec!["Spend more time".to_string()],
            vec!["Skip tests".to_string()],
        );
        let motivation_id = motivation.id.clone();
        agency::save_tradeoff(&motivation, &motivations_dir).unwrap();

        // Create an Agent entity pairing the role and motivation
        let agent_id = agency::content_hash_agent(&role_id, &motivation_id);
        let agent = agency::Agent {
            id: agent_id.clone(),
            role_id: role_id.clone(),
            tradeoff_id: motivation_id.clone(),
            name: "Test Agent".to_string(),
            performance: agency::PerformanceRecord::default(),
            lineage: agency::Lineage::default(),
            capabilities: Vec::new(),
            rate: None,
            capacity: None,
            trust_level: Default::default(),
            contact: None,
            executor: "claude".to_string(),
            preferred_model: None,
            preferred_provider: None,
            attractor_weight: 1.0,
            deployment_history: vec![],
            staleness_flags: vec![],
        };
        agency::save_agent(&agent, &agents_dir).unwrap();

        // Create a task with agent reference
        let mut task = make_test_task("task-1", "Test Task");
        task.agent = Some(agent_id);

        let vars = TemplateVars::from_task(&task, None, Some(&wg_dir));
        assert!(!vars.task_identity.is_empty());
        assert!(vars.task_identity.contains("Implementer"));
        assert!(vars.task_identity.contains("Spend more time")); // acceptable tradeoff
        assert!(vars.task_identity.contains("Skip tests")); // unacceptable tradeoff
        assert!(vars.task_identity.contains("Agent Identity"));
    }

    #[test]
    fn test_template_vars_identity_missing_agent_fallback() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        let agents_dir = wg_dir.join("agency").join("cache/agents");
        fs::create_dir_all(&agents_dir).unwrap();

        let mut task = make_test_task("task-1", "Test Task");
        task.agent = Some("nonexistent-agent-hash".to_string());

        // Should gracefully fallback to empty string when agent can't be found
        let vars = TemplateVars::from_task(&task, None, Some(&wg_dir));
        assert_eq!(vars.task_identity, "");
    }

    #[test]
    fn test_template_apply_with_identity() {
        let mut task = make_test_task("task-1", "Test Task");
        task.agent = None;
        let vars = TemplateVars::from_task(&task, None, None);

        let template = "Preamble\n{{task_identity}}\nTask: {{task_id}}";
        let result = vars.apply(template);
        assert_eq!(result, "Preamble\n\nTask: task-1");
    }

    // --- Error path tests for ExecutorConfig ---

    #[test]
    fn test_load_by_name_missing_config_file() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(wg_dir.join("executors")).unwrap();

        let result = ExecutorConfig::load_by_name(&wg_dir, "nonexistent");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Executor config not found: nonexistent"),
            "Expected 'not found' error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_load_by_name_missing_executors_directory() {
        let temp_dir = TempDir::new().unwrap();
        // .workgraph exists but executors/ subdirectory does not
        let wg_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();

        let result = ExecutorConfig::load_by_name(&wg_dir, "claude");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Executor config not found"),
            "Expected 'not found' error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_load_malformed_toml() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("bad.toml");
        fs::write(&config_path, "this is [not valid {{ toml").unwrap();

        let result = ExecutorConfig::load(&config_path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to parse executor config"),
            "Expected parse error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_load_missing_required_fields_no_executor_section() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("incomplete.toml");
        // Valid TOML but missing the [executor] section entirely
        fs::write(&config_path, "[something_else]\nkey = \"value\"\n").unwrap();

        let result = ExecutorConfig::load(&config_path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to parse executor config"),
            "Expected parse error for missing section, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_load_missing_required_fields_no_command() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("no_command.toml");
        // Has [executor] and type, but missing required 'command' field
        fs::write(&config_path, "[executor]\ntype = \"custom\"\n").unwrap();

        let result = ExecutorConfig::load(&config_path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to parse executor config"),
            "Expected parse error for missing command, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_load_missing_required_fields_no_type() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("no_type.toml");
        // Has [executor] and command, but missing required 'type' field
        fs::write(&config_path, "[executor]\ncommand = \"echo\"\n").unwrap();

        let result = ExecutorConfig::load(&config_path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to parse executor config"),
            "Expected parse error for missing type, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_load_nonexistent_file() {
        let result = ExecutorConfig::load(Path::new("/tmp/does_not_exist_ever_12345.toml"));
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to read executor config"),
            "Expected read error, got: {}",
            err_msg
        );
    }

    // --- TemplateVars edge case tests ---

    #[test]
    fn test_template_vars_nonexistent_workgraph_dir() {
        let task = make_test_task("task-1", "Test");
        // Pass a path that doesn't exist on disk — canonicalize will fail,
        // so working_dir should fall back to empty string.
        let fake_path = Path::new("/tmp/nonexistent_workgraph_dir_xyz_12345");
        let vars = TemplateVars::from_task(&task, None, Some(fake_path));
        assert_eq!(vars.working_dir, "");
    }

    #[test]
    fn test_template_vars_empty_task_description() {
        let mut task = make_test_task("task-1", "Test");
        task.description = None;
        let vars = TemplateVars::from_task(&task, None, None);
        assert_eq!(vars.task_description, "");
    }

    #[test]
    fn test_template_vars_special_characters() {
        let mut task = make_test_task("task-with-special", "Title with \"quotes\" & <tags>");
        task.description = Some("Desc with {{braces}} and $dollars and `backticks`".to_string());
        let vars = TemplateVars::from_task(&task, Some("Context with\nnewlines\tand\ttabs"), None);

        // Template application should be a literal substitution
        let result = vars.apply(
            "id={{task_id}} title={{task_title}} desc={{task_description}} ctx={{task_context}}",
        );
        assert_eq!(
            result,
            "id=task-with-special title=Title with \"quotes\" & <tags> desc=Desc with {{braces}} and $dollars and `backticks` ctx=Context with\nnewlines\tand\ttabs"
        );
    }

    #[test]
    fn test_template_apply_missing_variables_passthrough() {
        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, None, None);

        // Unrecognized placeholders should pass through unchanged
        let template = "{{task_id}} {{unknown_var}} {{another_unknown}}";
        let result = vars.apply(template);
        assert_eq!(result, "task-1 {{unknown_var}} {{another_unknown}}");
    }

    #[test]
    fn test_template_apply_no_placeholders() {
        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, None, None);

        let template = "Just a plain string with no placeholders";
        let result = vars.apply(template);
        assert_eq!(result, "Just a plain string with no placeholders");
    }

    #[test]
    fn test_template_apply_empty_string() {
        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, None, None);

        let result = vars.apply("");
        assert_eq!(result, "");
    }

    #[test]
    fn test_template_vars_working_dir_with_real_path() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();

        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, None, Some(&wg_dir));

        // working_dir should be the canonical parent of .workgraph
        let expected = temp_dir.path().canonicalize().unwrap();
        assert_eq!(vars.working_dir, expected.to_string_lossy().to_string());
    }

    // --- ExecutorRegistry error path tests ---

    #[test]
    fn test_registry_unknown_executor() {
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());

        let result = registry.load_config("totally_unknown_executor");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Unknown executor"),
            "Expected unknown executor error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_registry_load_from_file_overrides_default() {
        let temp_dir = TempDir::new().unwrap();
        let executors_dir = temp_dir.path().join("executors");
        fs::create_dir_all(&executors_dir).unwrap();

        // Write a custom claude config that overrides the default
        let custom_config = r#"
[executor]
type = "claude"
command = "my-custom-claude"
args = ["--custom-flag"]
"#;
        fs::write(executors_dir.join("claude.toml"), custom_config).unwrap();

        let registry = ExecutorRegistry::new(temp_dir.path());
        let config = registry.load_config("claude").unwrap();
        assert_eq!(config.executor.command, "my-custom-claude");
        assert_eq!(config.executor.args, vec!["--custom-flag"]);
    }

    #[test]
    fn test_registry_load_malformed_file_returns_error() {
        let temp_dir = TempDir::new().unwrap();
        let executors_dir = temp_dir.path().join("executors");
        fs::create_dir_all(&executors_dir).unwrap();
        fs::write(executors_dir.join("broken.toml"), "invalid toml {{{").unwrap();

        let registry = ExecutorRegistry::new(temp_dir.path());
        let result = registry.load_config("broken");
        assert!(result.is_err());
    }

    #[test]
    fn test_registry_init_idempotent() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();

        let registry = ExecutorRegistry::new(&wg_dir);

        // First init
        registry.init().unwrap();
        let claude_content_1 = fs::read_to_string(wg_dir.join("executors/claude.toml")).unwrap();

        // Second init should not fail and should not overwrite existing files
        registry.init().unwrap();
        let claude_content_2 = fs::read_to_string(wg_dir.join("executors/claude.toml")).unwrap();

        assert_eq!(claude_content_1, claude_content_2);
    }

    #[test]
    fn test_registry_default_config_claude_no_prompt_template() {
        // Built-in claude executor uses scope-based build_prompt() instead of a template
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());
        let config = registry.load_config("claude").unwrap();

        assert!(
            config.executor.prompt_template.is_none(),
            "Built-in claude config should have no prompt_template (uses build_prompt)"
        );
    }

    #[test]
    fn test_registry_default_config_codex_shape() {
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());
        let config = registry.load_config("codex").unwrap();

        assert_eq!(config.executor.executor_type, "codex");
        assert_eq!(config.executor.command, "codex");
        assert_eq!(
            config.executor.args,
            vec![
                "exec".to_string(),
                "--json".to_string(),
                "--skip-git-repo-check".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
            ]
        );
        assert_eq!(
            config.executor.working_dir,
            Some("{{working_dir}}".to_string())
        );
        assert!(config.executor.prompt_template.is_none());
    }

    #[test]
    fn test_registry_default_config_shell_has_env() {
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());
        let config = registry.load_config("shell").unwrap();

        assert_eq!(
            config.executor.env.get("TASK_ID"),
            Some(&"{{task_id}}".to_string())
        );
        assert_eq!(
            config.executor.env.get("TASK_TITLE"),
            Some(&"{{task_title}}".to_string())
        );
    }

    #[test]
    fn test_registry_default_config_default_executor() {
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());
        let config = registry.load_config("default").unwrap();

        assert_eq!(config.executor.executor_type, "default");
        assert_eq!(config.executor.command, "echo");
        assert_eq!(config.executor.args, vec!["Task: {{task_id}}"]);
    }

    // --- apply_templates edge cases ---

    #[test]
    fn test_apply_templates_no_prompt_template() {
        let config = ExecutorConfig {
            executor: ExecutorSettings {
                executor_type: "shell".to_string(),
                command: "bash".to_string(),
                args: vec!["-c".to_string(), "echo {{task_id}}".to_string()],
                env: HashMap::new(),
                prompt_template: None,
                working_dir: None,
                timeout: None,
                model: None,
            },
        };

        let task = make_test_task("t-1", "Test");
        let vars = TemplateVars::from_task(&task, None, None);
        let settings = config.apply_templates(&vars);

        assert!(settings.prompt_template.is_none());
        assert_eq!(settings.args, vec!["-c", "echo t-1"]);
    }

    #[test]
    fn test_apply_templates_no_working_dir() {
        let config = ExecutorConfig {
            executor: ExecutorSettings {
                executor_type: "test".to_string(),
                command: "cmd".to_string(),
                args: vec![],
                env: HashMap::new(),
                prompt_template: None,
                working_dir: None,
                timeout: None,
                model: None,
            },
        };

        let task = make_test_task("t-1", "Test");
        let vars = TemplateVars::from_task(&task, None, None);
        let settings = config.apply_templates(&vars);

        assert!(settings.working_dir.is_none());
    }

    #[test]
    fn test_apply_templates_multiple_env_vars() {
        let config = ExecutorConfig {
            executor: ExecutorSettings {
                executor_type: "test".to_string(),
                command: "cmd".to_string(),
                args: vec![],
                env: {
                    let mut env = HashMap::new();
                    env.insert("ID".to_string(), "{{task_id}}".to_string());
                    env.insert("TITLE".to_string(), "{{task_title}}".to_string());
                    env.insert("DESC".to_string(), "{{task_description}}".to_string());
                    env.insert("STATIC".to_string(), "no-template-here".to_string());
                    env
                },
                prompt_template: None,
                working_dir: None,
                timeout: None,
                model: None,
            },
        };

        let task = make_test_task("t-1", "My Task");
        let vars = TemplateVars::from_task(&task, None, None);
        let settings = config.apply_templates(&vars);

        assert_eq!(settings.env.get("ID"), Some(&"t-1".to_string()));
        assert_eq!(settings.env.get("TITLE"), Some(&"My Task".to_string()));
        assert_eq!(
            settings.env.get("DESC"),
            Some(&"Test description".to_string())
        );
        assert_eq!(
            settings.env.get("STATIC"),
            Some(&"no-template-here".to_string())
        );
    }

    // --- Identity resolution edge cases ---

    #[test]
    fn test_identity_agent_exists_but_role_missing() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        let roles_dir = wg_dir.join("agency").join("cache/roles");
        let motivations_dir = wg_dir.join("agency").join("primitives/tradeoffs");
        let agents_dir = wg_dir.join("agency").join("cache/agents");
        fs::create_dir_all(&roles_dir).unwrap();
        fs::create_dir_all(&motivations_dir).unwrap();
        fs::create_dir_all(&agents_dir).unwrap();

        // Create an agent that references a non-existent role
        let agent = agency::Agent {
            id: "test-agent-id".to_string(),
            role_id: "nonexistent-role".to_string(),
            tradeoff_id: "nonexistent-motivation".to_string(),
            name: "Broken Agent".to_string(),
            performance: agency::PerformanceRecord::default(),
            lineage: agency::Lineage::default(),
            capabilities: Vec::new(),
            rate: None,
            capacity: None,
            trust_level: Default::default(),
            contact: None,
            executor: "claude".to_string(),
            preferred_model: None,
            preferred_provider: None,
            attractor_weight: 1.0,
            deployment_history: vec![],
            staleness_flags: vec![],
        };
        agency::save_agent(&agent, &agents_dir).unwrap();

        let mut task = make_test_task("task-1", "Test");
        task.agent = Some("test-agent-id".to_string());

        // Should gracefully fall back to empty identity
        let vars = TemplateVars::from_task(&task, None, Some(&wg_dir));
        assert_eq!(vars.task_identity, "");
    }

    #[test]
    fn test_skills_preamble_empty_when_no_skill_file() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();

        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, None, Some(&wg_dir));
        assert_eq!(vars.skills_preamble, "");
    }

    #[test]
    fn test_skills_preamble_loaded_when_skill_file_exists() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();

        // Create the skill file at project_root/.claude/skills/using-superpowers/SKILL.md
        let skill_dir = temp_dir
            .path()
            .join(".claude")
            .join("skills")
            .join("using-superpowers");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "Use the Skill tool to invoke skills.",
        )
        .unwrap();

        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, None, Some(&wg_dir));
        assert!(vars.skills_preamble.contains("EXTREMELY_IMPORTANT"));
        assert!(
            vars.skills_preamble
                .contains("Use the Skill tool to invoke skills.")
        );
    }

    #[test]
    fn test_skills_preamble_strips_yaml_frontmatter() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();

        let skill_dir = temp_dir
            .path()
            .join(".claude")
            .join("skills")
            .join("using-superpowers");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\ntitle: Skill\n---\nActual content here.",
        )
        .unwrap();

        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, None, Some(&wg_dir));
        assert!(vars.skills_preamble.contains("Actual content here."));
        // The frontmatter itself should not appear in the preamble body
        assert!(!vars.skills_preamble.contains("title: Skill"));
    }

    #[test]
    fn test_template_vars_include_loop_info() {
        let mut task = make_test_task("task-1", "Looping Task");
        task.loop_iteration = 2;
        task.cycle_config = Some(crate::graph::CycleConfig {
            max_iterations: 3,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        let vars = TemplateVars::from_task(&task, None, None);

        assert!(vars.task_loop_info.contains("iteration 3"));
    }

    #[test]
    fn test_template_vars_empty_loop_info_for_non_loop_tasks() {
        let task = make_test_task("task-1", "Normal Task");
        let vars = TemplateVars::from_task(&task, None, None);
        assert_eq!(vars.task_loop_info, "");
    }

    #[test]
    fn test_build_prompt_contains_converged_for_task_scope() {
        // build_prompt with task scope should include --converged in workflow section
        let task = make_test_task("task-1", "Looping Task");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let ctx = ScopeContext::default();
        let prompt = build_prompt(&vars, ContextScope::Task, &ctx);
        assert!(
            prompt.contains("--converged"),
            "Task-scope prompt should mention --converged"
        );
    }

    #[test]
    fn test_build_prompt_no_workflow_for_clean_scope() {
        // build_prompt with clean scope should NOT include workflow sections
        let task = make_test_task("task-1", "Clean Task");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let ctx = ScopeContext::default();
        let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);
        assert!(
            !prompt.contains("## Required Workflow"),
            "Clean-scope prompt should not include Required Workflow"
        );
        assert!(
            !prompt.contains("## Graph Patterns"),
            "Clean-scope prompt should not include Graph Patterns"
        );
        assert!(
            !prompt.contains("## CRITICAL"),
            "Clean-scope prompt should not include CRITICAL CLI section"
        );
        // But should still have task info
        assert!(prompt.contains("task-1"));
        assert!(prompt.contains("Clean Task"));
    }

    #[test]
    fn test_default_amplifier_no_prompt_template() {
        // Built-in amplifier executor uses scope-based build_prompt() instead of a template
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());
        let config = registry.load_config("amplifier").unwrap();
        assert!(
            config.executor.prompt_template.is_none(),
            "Built-in amplifier config should have no prompt_template (uses build_prompt)"
        );
    }

    #[test]
    fn test_template_apply_loop_info_substitution() {
        let mut task = make_test_task("task-1", "Loop Task");
        task.loop_iteration = 1;
        task.cycle_config = Some(crate::graph::CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        let vars = TemplateVars::from_task(&task, None, None);
        let template = "Before\n{{task_loop_info}}\nAfter";
        let result = vars.apply(template);

        assert!(result.contains("Before"));
        assert!(result.contains("After"));
        assert!(!result.contains("{{task_loop_info}}"));
    }

    // --- build_prompt scope tests ---

    #[test]
    fn test_build_prompt_clean_scope_minimal() {
        let task = make_test_task("task-1", "Clean Task");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let ctx = ScopeContext::default();
        let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);

        // Should include task info
        assert!(prompt.contains("# Task Assignment"));
        assert!(prompt.contains("task-1"));
        assert!(prompt.contains("Clean Task"));
        assert!(prompt.contains("Test description"));
        assert!(prompt.contains("dep context"));
        assert!(prompt.contains("Begin working on the task now."));

        // Should NOT include workflow/graph sections
        assert!(!prompt.contains("## Required Workflow"));
        assert!(!prompt.contains("## Graph Patterns"));
        assert!(!prompt.contains("## Reusable Workflow Functions"));
        assert!(!prompt.contains("## CRITICAL"));
        assert!(!prompt.contains("## Additional Context"));
        assert!(!prompt.contains("## System Awareness"));
    }

    #[test]
    fn test_build_prompt_task_scope_includes_workflow() {
        let task = make_test_task("task-1", "Task Scope");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let ctx = ScopeContext {
            downstream_info: "## Downstream\n- dt-1: \"Consumer\"".to_string(),
            tags_skills_info: "- **Tags:** rust, impl".to_string(),
            ..Default::default()
        };
        let prompt = build_prompt(&vars, ContextScope::Task, &ctx);

        // Should include workflow sections
        assert!(prompt.contains("## Required Workflow"));
        assert!(prompt.contains("wg log task-1")); // {{task_id}} substituted
        assert!(prompt.contains("wg done task-1"));
        assert!(prompt.contains("## Graph Patterns"));
        assert!(prompt.contains("## Reusable Workflow Functions"));
        assert!(prompt.contains("## CRITICAL: Use wg CLI"));
        assert!(prompt.contains("wg add \"title\" --after task-1")); // {{task_id}} substituted
        assert!(prompt.contains("## Additional Context")); // R2

        // Should include R1 and R4
        assert!(prompt.contains("## Downstream"));
        assert!(prompt.contains("Consumer"));
        assert!(prompt.contains("rust, impl"));

        // Should NOT include graph/full sections
        assert!(!prompt.contains("## System Awareness"));
        assert!(!prompt.contains("## Project\n"));
        assert!(!prompt.contains("## Full Graph Summary"));
        assert!(!prompt.contains("CLAUDE.md"));
    }

    #[test]
    fn test_build_prompt_graph_scope_includes_project_and_summary() {
        let task = make_test_task("task-1", "Graph Scope");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let ctx = ScopeContext {
            project_description: "A project for testing.".to_string(),
            graph_summary: "## Graph Status\n\n5 tasks".to_string(),
            ..Default::default()
        };
        let prompt = build_prompt(&vars, ContextScope::Graph, &ctx);

        // Should include task+ sections
        assert!(prompt.contains("## Required Workflow"));
        assert!(prompt.contains("## Graph Patterns"));

        // Should include graph sections
        assert!(prompt.contains("## Project\n\nA project for testing."));
        assert!(prompt.contains("## Graph Status\n\n5 tasks"));

        // Should NOT include full sections
        assert!(!prompt.contains("## System Awareness"));
        assert!(!prompt.contains("## Full Graph Summary"));
        assert!(!prompt.contains("CLAUDE.md"));
    }

    #[test]
    fn test_build_prompt_full_scope_includes_everything() {
        let task = make_test_task("task-1", "Full Scope");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let ctx = ScopeContext {
            downstream_info: "## Downstream\n- dt-1".to_string(),
            tags_skills_info: "- **Tags:** meta".to_string(),
            project_description: "My project".to_string(),
            graph_summary: "## Graph Status\n\n10 tasks".to_string(),
            full_graph_summary: "## Full Graph Summary\n\n- task-a [done]".to_string(),
            claude_md_content: "Always use bun.".to_string(),
            queued_messages: String::new(),
            previous_attempt_context: String::new(),
            ..ScopeContext::default()
        };
        let prompt = build_prompt(&vars, ContextScope::Full, &ctx);

        // Should include all sections
        assert!(prompt.contains("## System Awareness"));
        assert!(prompt.contains("## Required Workflow"));
        assert!(prompt.contains("## Graph Patterns"));
        assert!(prompt.contains("## Project\n\nMy project"));
        assert!(prompt.contains("## Graph Status"));
        assert!(prompt.contains("## Full Graph Summary"));
        assert!(prompt.contains("## Project Instructions (CLAUDE.md)\n\nAlways use bun."));
        assert!(prompt.contains("## Downstream"));
        assert!(prompt.contains("meta"));
    }

    #[test]
    fn test_build_prompt_includes_loop_info() {
        let mut task = make_test_task("task-1", "Loop Task");
        task.loop_iteration = 2;
        task.cycle_config = Some(crate::graph::CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        let vars = TemplateVars::from_task(&task, None, None);
        let ctx = ScopeContext::default();
        let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);

        // Loop info should appear even at clean scope
        assert!(prompt.contains("iteration 3"));
        assert!(prompt.contains("--converged"));
    }

    #[test]
    fn test_build_prompt_includes_identity() {
        let task = make_test_task("task-1", "Identity Task");
        let mut vars = TemplateVars::from_task(&task, None, None);
        vars.task_identity = "## Agent Identity\n\nRole: implementer\n".to_string();

        let ctx = ScopeContext::default();
        let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);

        assert!(prompt.contains("## Agent Identity"));
        assert!(prompt.contains("Role: implementer"));
    }

    #[test]
    fn test_build_prompt_includes_skills_preamble() {
        let task = make_test_task("task-1", "Skills Task");
        let mut vars = TemplateVars::from_task(&task, None, None);
        vars.skills_preamble =
            "<EXTREMELY_IMPORTANT>\nUse skills.\n</EXTREMELY_IMPORTANT>\n".to_string();

        let ctx = ScopeContext::default();
        let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);

        assert!(prompt.contains("EXTREMELY_IMPORTANT"));
        assert!(prompt.contains("Use skills."));
    }

    #[test]
    fn test_build_prompt_empty_scope_context_omits_sections() {
        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, Some("dep ctx"), None);
        let ctx = ScopeContext::default(); // all empty

        let prompt = build_prompt(&vars, ContextScope::Full, &ctx);

        // Should still have system preamble and workflow even with empty ctx
        assert!(prompt.contains("## System Awareness"));
        assert!(prompt.contains("## Required Workflow"));

        // But should NOT have empty project/graph sections
        assert!(!prompt.contains("## Project\n"));
        assert!(!prompt.contains("## Graph Status"));
        assert!(!prompt.contains("## Full Graph Summary"));
        assert!(!prompt.contains("CLAUDE.md"));
    }

    #[test]
    fn test_section_constants_contain_expected_content() {
        assert!(REQUIRED_WORKFLOW_SECTION.contains("wg log {{task_id}}"));
        assert!(REQUIRED_WORKFLOW_SECTION.contains("wg done {{task_id}}"));
        assert!(REQUIRED_WORKFLOW_SECTION.contains("wg fail {{task_id}}"));
        assert!(REQUIRED_WORKFLOW_SECTION.contains("wg artifact {{task_id}}"));
        assert!(REQUIRED_WORKFLOW_SECTION.contains("--converged"));
        assert!(REQUIRED_WORKFLOW_SECTION.contains("git commit"));
        assert!(REQUIRED_WORKFLOW_SECTION.contains("git push"));

        assert!(GRAPH_PATTERNS_SECTION.contains("Golden rule"));
        assert!(GRAPH_PATTERNS_SECTION.contains("pipeline"));
        assert!(GRAPH_PATTERNS_SECTION.contains("cargo install --path"));

        assert!(REUSABLE_FUNCTIONS_SECTION.contains("wg func list"));
        assert!(REUSABLE_FUNCTIONS_SECTION.contains("wg func apply"));

        assert!(CRITICAL_WG_CLI_SECTION.contains("NEVER use built-in TaskCreate"));
        assert!(CRITICAL_WG_CLI_SECTION.contains("wg add \"title\" --after {{task_id}}"));
    }

    #[test]
    fn test_build_prompt_includes_verify_when_present() {
        let mut task = make_test_task("task-1", "Test");
        task.verify = Some("run cargo test and confirm all pass".to_string());
        let vars = TemplateVars::from_task(&task, Some("dep ctx"), None);
        let ctx = ScopeContext::default();

        // Verify section should appear at all scopes (including clean)
        let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);
        assert!(prompt.contains("## Verification Required"));
        assert!(prompt.contains("run cargo test and confirm all pass"));
        assert!(prompt.contains("Before marking done, you MUST verify:"));
    }

    #[test]
    fn test_build_prompt_omits_verify_when_absent() {
        let task = make_test_task("task-1", "Test");
        assert!(task.verify.is_none());
        let vars = TemplateVars::from_task(&task, Some("dep ctx"), None);
        let ctx = ScopeContext::default();

        let prompt = build_prompt(&vars, ContextScope::Task, &ctx);
        assert!(!prompt.contains("## Verification Required"));
    }

    #[test]
    fn test_template_vars_verify_from_task() {
        let mut task = make_test_task("task-1", "Test");
        task.verify = Some("check output format".to_string());
        let vars = TemplateVars::from_task(&task, None, None);
        assert_eq!(vars.task_verify, Some("check output format".to_string()));

        let task2 = make_test_task("task-2", "Test2");
        let vars2 = TemplateVars::from_task(&task2, None, None);
        assert_eq!(vars2.task_verify, None);
    }

    #[test]
    fn test_build_prompt_triage_mode_injected_when_failed_deps() {
        let task = make_test_task("task-1", "Downstream task");
        let mut vars = TemplateVars::from_task(&task, Some("dep context"), None);
        vars.has_failed_deps = true;
        vars.failed_deps_info = "- dep-a: \"Build parser\" — Reason: cargo test failed".to_string();
        let ctx = ScopeContext::default();
        let prompt = build_prompt(&vars, ContextScope::Task, &ctx);

        assert!(
            prompt.contains("TRIAGE MODE"),
            "Prompt should contain TRIAGE MODE header"
        );
        assert!(
            prompt.contains("dep-a"),
            "Prompt should include the failed dep info"
        );
        assert!(
            prompt.contains("Failed Dependency Protocol"),
            "Prompt should include the protocol section"
        );
        assert!(
            prompt.contains("wg requeue task-1"),
            "Prompt should include task-specific requeue command"
        );
        assert!(
            prompt.contains("wg retry"),
            "Prompt should include retry instruction"
        );
    }

    #[test]
    fn test_build_prompt_no_triage_when_no_failed_deps() {
        let task = make_test_task("task-1", "Normal task");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let ctx = ScopeContext::default();
        let prompt = build_prompt(&vars, ContextScope::Task, &ctx);

        assert!(
            !prompt.contains("TRIAGE MODE"),
            "Prompt should NOT contain TRIAGE MODE when no failed deps"
        );
        assert!(
            !prompt.contains("Failed Dependency Protocol"),
            "Prompt should NOT contain triage protocol when no failed deps"
        );
    }

    #[test]
    fn test_build_prompt_injects_wg_guide_for_non_claude() {
        let task = make_test_task("task-1", "Native model task");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let ctx = ScopeContext {
            wg_guide_content: "This is the wg guide for non-Claude models.".to_string(),
            ..Default::default()
        };
        let prompt = build_prompt(&vars, ContextScope::Task, &ctx);

        assert!(
            prompt.contains("## Workgraph Usage Guide"),
            "Task scope should include wg guide when content is present"
        );
        assert!(
            prompt.contains("This is the wg guide for non-Claude models."),
            "Guide content should appear in the prompt"
        );
    }

    #[test]
    fn test_build_prompt_no_wg_guide_for_claude() {
        let task = make_test_task("task-1", "Claude model task");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        // Empty wg_guide_content simulates Claude executor (guide not injected)
        let ctx = ScopeContext::default();
        let prompt = build_prompt(&vars, ContextScope::Task, &ctx);

        assert!(
            !prompt.contains("## Workgraph Usage Guide"),
            "Task scope should NOT include wg guide when content is empty"
        );
    }

    #[test]
    fn test_build_prompt_wg_guide_not_injected_at_clean_scope() {
        let task = make_test_task("task-1", "Clean scope task");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let ctx = ScopeContext {
            wg_guide_content: "Guide content".to_string(),
            ..Default::default()
        };
        let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);

        assert!(
            !prompt.contains("## Workgraph Usage Guide"),
            "Clean scope should NOT include wg guide"
        );
    }

    #[test]
    fn test_default_wg_guide_covers_key_topics() {
        let guide = DEFAULT_WG_GUIDE;

        // Must cover --after for dependency edges
        assert!(
            guide.contains("--after"),
            "Guide must explain --after for dependencies"
        );

        // Must cover pending-validation gate (--validation llm or --verify)
        assert!(
            guide.contains("pending-validation") || guide.contains("--validation"),
            "Guide must explain the pending-validation gate"
        );

        // Must cover wg log, wg done, wg fail
        assert!(guide.contains("wg log"), "Guide must cover wg log");
        assert!(guide.contains("wg done"), "Guide must cover wg done");
        assert!(guide.contains("wg fail"), "Guide must cover wg fail");

        // Must cover decomposition guidance
        assert!(
            guide.contains("Decompose"),
            "Guide must cover when to decompose"
        );
    }

    // --- Adaptive decomposition intelligence tests ---

    #[test]
    fn test_classify_atomic_single_function() {
        assert_eq!(
            classify_task_complexity("Fix bug in the parse function"),
            TaskComplexity::Atomic
        );
    }

    #[test]
    fn test_classify_atomic_typo() {
        assert_eq!(
            classify_task_complexity("Fix typo in README"),
            TaskComplexity::Atomic
        );
    }

    #[test]
    fn test_classify_atomic_rename() {
        assert_eq!(
            classify_task_complexity("Rename variable foo to bar"),
            TaskComplexity::Atomic
        );
    }

    #[test]
    fn test_classify_multi_step_pipeline() {
        assert_eq!(
            classify_task_complexity("Build pipeline to parse, transform, and load data"),
            TaskComplexity::MultiStep
        );
    }

    #[test]
    fn test_classify_multi_step_sequential_signals() {
        assert_eq!(
            classify_task_complexity(
                "First, parse the input. Then, validate it. Finally, write the output."
            ),
            TaskComplexity::MultiStep
        );
    }

    #[test]
    fn test_classify_multi_step_bullet_list() {
        let desc = "Implement the feature:\n\
                     - Add the model\n\
                     - Add the controller\n\
                     - Add the view\n\
                     - Write tests";
        assert_eq!(classify_task_complexity(desc), TaskComplexity::MultiStep);
    }

    #[test]
    fn test_classify_multi_step_integration() {
        assert_eq!(
            classify_task_complexity("Integrate the auth module with the user service"),
            TaskComplexity::MultiStep
        );
    }

    #[test]
    fn test_classify_short_ambiguous_defaults_atomic() {
        assert_eq!(
            classify_task_complexity("Do the thing"),
            TaskComplexity::Atomic
        );
    }

    #[test]
    fn test_classify_long_ambiguous_defaults_multi_step() {
        let long_desc = "a".repeat(201);
        assert_eq!(
            classify_task_complexity(&long_desc),
            TaskComplexity::MultiStep
        );
    }

    #[test]
    fn test_adaptive_guidance_atomic_task() {
        let guidance = build_decomposition_guidance("Fix typo in the README", "fix-typo", 10, 8);
        assert!(
            guidance.contains("single-step task"),
            "Atomic task should get single-step guidance"
        );
        assert!(
            !guidance.contains("Decomposition Templates"),
            "Atomic task should NOT get decomposition templates"
        );
        assert!(
            guidance.contains("Guardrails"),
            "Both types should include guardrails"
        );
    }

    #[test]
    fn test_adaptive_guidance_multi_step_task() {
        let guidance = build_decomposition_guidance(
            "Build pipeline to parse and transform data, then write output",
            "build-pipeline",
            10,
            8,
        );
        assert!(
            guidance.contains("multi-step task"),
            "Multi-step task should get multi-step guidance"
        );
        assert!(
            guidance.contains("Decomposition Templates"),
            "Multi-step task should get decomposition templates"
        );
        assert!(
            guidance.contains("Pipeline"),
            "Templates should include Pipeline pattern"
        );
        assert!(
            guidance.contains("Fan-out-merge"),
            "Templates should include Fan-out-merge pattern"
        );
        assert!(
            guidance.contains("Iterate-until-pass"),
            "Templates should include Iterate-until-pass pattern"
        );
        assert!(
            guidance.contains("--after build-pipeline"),
            "Templates should use the task_id in --after examples"
        );
    }

    #[test]
    fn test_adaptive_guidance_includes_task_id() {
        let guidance = build_decomposition_guidance("Fix typo in README", "my-task-42", 10, 8);
        assert!(
            guidance.contains("my-task-42"),
            "Guidance should reference the task ID"
        );
    }

    #[test]
    fn test_adaptive_guidance_includes_guardrails() {
        let guidance = build_decomposition_guidance("Build a system", "sys-task", 15, 12);
        assert!(
            guidance.contains("**15**"),
            "Guardrails should show max_child_tasks"
        );
        assert!(
            guidance.contains("**12**"),
            "Guardrails should show max_task_depth"
        );
    }

    #[test]
    fn test_build_prompt_uses_adaptive_guidance_when_enabled() {
        let mut task = make_test_task("task-1", "Build pipeline");
        task.description = Some("Build pipeline to parse, transform, and write data".to_string());
        let vars = TemplateVars::from_task(&task, Some("dep ctx"), None);
        let ctx = ScopeContext {
            decomp_guidance: true,
            ..Default::default()
        };
        let prompt = build_prompt(&vars, ContextScope::Task, &ctx);

        assert!(
            prompt.contains("multi-step task"),
            "Adaptive guidance should classify pipeline task as multi-step"
        );
        assert!(
            prompt.contains("Decomposition Templates"),
            "Adaptive guidance should include templates for multi-step task"
        );
    }

    #[test]
    fn test_build_prompt_uses_static_guidance_when_disabled() {
        let task = make_test_task("task-1", "Build pipeline");
        let vars = TemplateVars::from_task(&task, Some("dep ctx"), None);
        let ctx = ScopeContext {
            decomp_guidance: false,
            ..Default::default()
        };
        let prompt = build_prompt(&vars, ContextScope::Task, &ctx);

        assert!(
            !prompt.contains("multi-step task"),
            "Static guidance should NOT contain adaptive classification"
        );
        assert!(
            prompt.contains("Fanout is a tool, not a default"),
            "Static guidance should contain AUTOPOIETIC_GUIDANCE text"
        );
    }

    #[test]
    fn test_build_prompt_atomic_adaptive_no_templates() {
        let mut task = make_test_task("task-1", "Fix typo");
        task.description = Some("Fix typo in the README file".to_string());
        let vars = TemplateVars::from_task(&task, Some("dep ctx"), None);
        let ctx = ScopeContext {
            decomp_guidance: true,
            ..Default::default()
        };
        let prompt = build_prompt(&vars, ContextScope::Task, &ctx);

        assert!(
            prompt.contains("single-step task"),
            "Adaptive guidance should classify fix-typo as atomic"
        );
        assert!(
            !prompt.contains("Decomposition Templates"),
            "Atomic tasks should NOT get decomposition templates"
        );
    }

    #[test]
    fn test_classify_mixed_signals_multi_step_wins_tie() {
        // "rename" is atomic, "refactor" is multi-step => multi-step wins ties
        assert_eq!(
            classify_task_complexity("Rename and refactor the module"),
            TaskComplexity::MultiStep
        );
    }
}

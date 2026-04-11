# Prompt Construction Analysis: Claude vs Native Executor

## Overview

All built-in executors (claude, codex, amplifier, native) share the **same `build_prompt()` function** for assembling the task prompt. The differences lie in:

1. **How the prompt reaches the model** (delivery mechanism)
2. **What supplementary context is injected** (wg guide, CLAUDE.md, bundle suffixes)
3. **How exec modes affect tool access and prompt structure** (bare/light/full)
4. **What the model "sees" beyond the workgraph prompt** (Claude CLI's own context injection)

---

## 1. Shared Prompt Assembly: `build_prompt()`

**File:** `src/service/executor.rs:709-880`

All executor types use the same `build_prompt(vars, scope, scope_ctx)` function. The prompt is assembled from sections gated by `ContextScope` (clean < task < graph < full):

| Scope Level | Sections Included |
|-------------|-------------------|
| **Clean** | skills_preamble, task assignment header, identity, task details, pattern keywords glossary, verification criteria, dependency context, triage mode, loop info |
| **Task** | + discovered tests, tags/skills, queued messages, downstream awareness, wg guide (non-Claude only), workflow commands, git hygiene, message polling, ethos, decomposition guidance, research hints, graph patterns, reusable functions, critical wg CLI warning, wg context hint |
| **Graph** | + project description, 1-hop neighborhood subgraph summary |
| **Full** | + system awareness preamble, full graph summary, CLAUDE.md content |

**Key:** The `build_prompt()` output is **identical** for all executor types given the same `TemplateVars`, scope, and `ScopeContext`. The differences come from how `ScopeContext` is populated before the call.

---

## 2. Prompt Delivery per Executor Type

**File:** `src/commands/spawn/execution.rs:787-1040` (`build_inner_command()`)

### Claude Executor (type: "claude")
- **Command:** `claude --print --verbose --permission-mode bypassPermissions --output-format stream-json`
- **Prompt delivery:** Written to `prompt.txt`, piped via `cat prompt.txt | claude ...`
- **Exec modes:**
  - **full** (line 905): All tools available, `--disallowedTools Agent`
  - **light** (line 872): `--allowedTools Bash(wg:*),Read,Glob,Grep,WebFetch,WebSearch`, `--disallowedTools Edit,Write,NotebookEdit,Agent`
  - **bare** (line 828): `--tools Bash(wg:*)`, `--allowedTools Bash(wg:*)`, prompt via `--system-prompt`, task description piped as user message
  - **resume** (line 801): `--resume <session_id>`, checkpoint as follow-up message

### Codex Executor (type: "codex")
- **Command:** `codex exec --json --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox`
- **Prompt delivery:** Written to `prompt.txt`, piped via stdin
- **No exec mode branching** — single code path

### Amplifier Executor (type: "amplifier")
- **Command:** `amplifier run --mode single --output-format text`
- **Prompt delivery:** Written to `prompt.txt`, piped via stdin
- **Model format:** Supports `provider:model` split into `-p provider -m model`
- **No exec mode branching** — single code path

### Native Executor (type: "native")
- **Command:** `wg native-exec --prompt-file <path> --exec-mode <mode> --task-id <id>`
- **Prompt delivery:** Written to `prompt.txt`, passed as `--prompt-file` argument
- **Additional CLI args:** `--model`, `--provider`, `--endpoint-name`, `--endpoint-url`, `--api-key`
- **Exec modes handled by bundle system** (see section 4)

### Shell Executor (type: "shell")
- **Command:** `bash -c <task.exec>`
- **No prompt** — executes the shell command directly
- **No exec mode branching**

---

## 3. Critical Differences: wg Guide and CLAUDE.md Propagation

### Difference 1: wg Usage Guide Injection (native-only)

**File:** `src/commands/spawn/execution.rs:312-317`

```rust
// Claude agents get this context from CLAUDE.md; native executor models need it
// explicitly injected into the prompt.
if settings.executor_type == "native" {
    scope_ctx.wg_guide_content = super::context::read_wg_guide(dir);
}
```

The `wg_guide_content` is **only populated for native executors**. It contains the `DEFAULT_WG_GUIDE` constant (`src/service/executor.rs:535-597`) — a concise guide to wg commands, task lifecycle, dependencies, and environment variables. This guide is rendered in `build_prompt()` at task+ scope as a `## Workgraph Usage Guide` section (line 800-805).

**For Claude executors:** The wg guide is NOT injected because Claude agents read `CLAUDE.md` from the project root, which contains equivalent (and more detailed) workgraph instructions. The Claude CLI loads `CLAUDE.md` automatically as part of its own context injection — this happens **outside** the workgraph prompt.

**For amplifier/codex executors:** The wg guide is also NOT injected. These executors receive only what `build_prompt()` generates. If their context scope is < full, they won't see CLAUDE.md content either, leaving them without wg usage documentation.

### Difference 2: CLAUDE.md Content in Prompt vs Implicit Loading

**File:** `src/commands/spawn/context.rs:540-553` (`read_claude_md()`)
**File:** `src/service/executor.rs:846-851` (injection in `build_prompt()`)

CLAUDE.md content is included in the prompt **only at `full` scope**:

```rust
// Full scope: CLAUDE.md content
if scope >= ContextScope::Full && !ctx.claude_md_content.is_empty() {
    parts.push(format!(
        "## Project Instructions (CLAUDE.md)\n\n{}",
        ctx.claude_md_content
    ));
}
```

For **Claude executor agents**, CLAUDE.md is loaded **twice**:
1. At full scope, it's included in the workgraph-assembled prompt
2. The Claude CLI **independently** loads CLAUDE.md from the working directory as part of its system context

For **native/codex/amplifier agents** at scopes below `full` (clean, task, graph), CLAUDE.md content is **absent** from the prompt entirely. The native executor doesn't have an independent CLAUDE.md loading mechanism.

### Difference 3: System Awareness Preamble (full scope only)

**File:** `src/service/executor.rs:148-157`

The `SYSTEM_AWARENESS_PREAMBLE` (explaining coordinator, agency, cycles, trace functions, context scopes) is only injected at `full` scope. Agents at lower scopes don't get this orientation.

---

## 4. Native Executor Bundle System

**File:** `src/executor/native/bundle.rs:1-155`
**File:** `src/commands/native_exec.rs:64-77`

The native executor has a **bundle system** that adds a `system_prompt_suffix` to the workgraph prompt:

```rust
let system_prompt = if system_suffix.is_empty() {
    prompt  // Just the build_prompt() output
} else {
    format!("{}\n\n{}", prompt, system_suffix)
};
```

Built-in bundles and their suffixes:

| Exec Mode | Bundle | Suffix | Tools |
|-----------|--------|--------|-------|
| bare | `Bundle::bare()` | "You are a lightweight agent. Use only wg tools to inspect and manage tasks." | wg_show, wg_list, wg_add, wg_done, wg_fail, wg_log, wg_artifact |
| light | `Bundle::research()` | "You are a research agent. Report findings, do not modify files." | read_file, glob, grep, bash, wg_* |
| full | `Bundle::implementer()` | *(empty)* | All tools (`*`) |

The Claude executor achieves equivalent tool filtering through `--allowedTools` / `--disallowedTools` CLI flags rather than bundle-based tool filtering.

---

## 5. Executor Default Configurations

**File:** `src/service/executor.rs:1246-1361` (`default_config()`)

| Executor | Command | Default Args | Working Dir | Default Timeout |
|----------|---------|-------------|-------------|-----------------|
| claude | `claude` | `--print --verbose --permission-mode bypassPermissions --output-format stream-json` | `{{working_dir}}` | None |
| codex | `codex` | `exec --json --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox` | `{{working_dir}}` | None |
| amplifier | `amplifier` | `run --mode single --output-format text` | `{{working_dir}}` | 600s |
| native | `wg` | `native-exec` | `{{working_dir}}` | None |
| shell | `bash` | `-c {{task_context}}` | None | None |

---

## 6. Environment Variables (Shared Across All Executors)

**File:** `src/commands/spawn/execution.rs:499-554`

All executors receive:
- `WG_TASK_ID`, `WG_AGENT_ID`, `WG_EXECUTOR_TYPE`
- `WG_TASK_TIMEOUT_SECS`, `WG_SPAWN_EPOCH`
- `WG_USER`, `WG_MODEL`
- `WG_ENDPOINT`, `WG_ENDPOINT_NAME`, `WG_LLM_PROVIDER`, `WG_ENDPOINT_URL`, `WG_API_KEY`
- Worktree vars: `WG_WORKTREE_PATH`, `WG_BRANCH`, `WG_PROJECT_ROOT`

---

## 7. Side-by-Side Prompt Comparison

### Scenario: Task at `task` scope with no failed deps

**Claude executor receives (from `build_prompt()`):**
```
# Task Assignment

You are an AI agent working on a task in a workgraph project.

## Agent Identity
[identity block]

## Your Task
- **ID:** example-task
- **Title:** Example task
- **Description:** Do something

## Verification Required
[verify criteria]

## Discovered Test Files
[test files]

## Context from Dependencies
[dependency context]

## Required Workflow
[wg workflow commands with task_id substitution]

## Git Hygiene
[shared repo rules]

## Messages
[message polling instructions]

## The Graph is Alive
[ethos section]

## Task Decomposition
[autopoietic guidance]

## Research Before Implementing
[research hints]

## Graph Patterns
[pipeline/diamond/scatter-gather/loop patterns]

## Reusable Workflow Functions
[wg func commands]

## CRITICAL: Use wg CLI, NOT built-in tools
[CLI warning]

## Additional Context
[context hints]

Begin working on the task now.
```

**Plus** Claude CLI independently injects: CLAUDE.md, git status, recent commits, system reminders, user memory files, keybindings, available skills

**Native executor receives (same build_prompt output) PLUS:**
```
## Workgraph Usage Guide

**Workgraph (wg)** is a task coordination graph for AI agents...
[DEFAULT_WG_GUIDE: commands table, dependency syntax, verify syntax, env vars]
```

**Plus** (from bundle system) system_prompt_suffix (e.g., "You are a research agent..." for light mode)

**But native does NOT get:** Claude CLI's independent CLAUDE.md loading, git status injection, system reminders, memory files, or skill listings

---

## 8. Key Findings

1. **Prompt content is identical across executors** — `build_prompt()` produces the same output given the same inputs. The real differences are in **what surrounds the prompt**.

2. **Claude executor double-loads CLAUDE.md** — once in the workgraph prompt (at full scope) and once via Claude CLI's own mechanism. This is redundant but harmless.

3. **Native executor gets wg guide; Claude doesn't** — The native executor needs explicit wg CLI documentation because it lacks CLAUDE.md auto-loading. This is the single code-path difference in `ScopeContext` population (`execution.rs:312-317`).

4. **Codex and amplifier get neither** — They don't receive the wg guide (not `"native"` type) and don't have their own CLAUDE.md mechanism. At scopes below `full`, they lack wg documentation.

5. **Claude's "invisible" context is substantial** — The Claude CLI injects CLAUDE.md, git status, recent commits, system reminders, and available skills/memory **beyond** what `build_prompt()` produces. Native/codex/amplifier agents see **only** the `build_prompt()` output plus their executor-specific additions.

6. **Tool filtering differs by mechanism** — Claude uses `--allowedTools`/`--disallowedTools` CLI flags; native uses the bundle system's `filter_registry()`. The net effect is similar but the implementations are distinct.

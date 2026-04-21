# Research: Agent Self-Discovery of `wg`

**Task:** research-agent-wg-awareness  
**Date:** 2026-04-20  

---

## 1. Current Agent-Awareness Mechanisms per Condition

The Harbor adapter (`terminal-bench/wg/adapter.py`) implements seven conditions (A through G-smart), each giving the agent a different level of wg awareness:

| Condition | wg Binary | wg Tools | Skill/Prompt Injection | Agency | Multi-Agent | Awareness Level |
|-----------|-----------|----------|----------------------|--------|-------------|-----------------|
| **A** (control) | Yes, in container | **Excluded** via custom bundle | None — bare system prompt | None | 1 agent | Zero — agent doesn't know wg exists |
| **B** | Yes | Full (in-process tools) | Brief tool descriptions only | None | 1 agent | Tool names visible, no usage guidance |
| **C** | Yes | Full | **Skill prompt** with decomposition heuristics, planning phase, wg_log mandate | None | 1 agent | Taught *when* and *why* to use wg |
| **D** | Yes | Full | Task context scope | `(programmer, careful)` | 1 agent | Agency identity assigned |
| **E** | Yes | Full | Graph context scope | `(architect, thorough)` | 1 agent | Broader graph awareness |
| **F** | Yes | Full | **Distilled context** (`CONDITION_F_MEMORY`) + `WG_QUICK_GUIDE` | None | 1 agent | Full project knowledge parity |
| **G** | Yes | Architect bundle (read-only + wg tools) | **Autopoietic meta-prompt** (`CONDITION_G_META_PROMPT`) | None | Up to 8 | Agent builds its own workgraph |
| **G-smart** | Yes | Full (can implement directly) | **Smart fanout meta-prompt** (try-first, decompose-if-needed) | None | Up to 4 | Direct implementation preferred, fanout available |

### Key Finding: Binary Upload is Universal

All conditions — even Condition A — upload the wg binary into the Docker container (`adapter.py:1476-1477`):
```python
await environment.upload_file(wg_bin, "/usr/local/bin/wg")
await environment.exec(command="chmod +x /usr/local/bin/wg")
```

The differentiation is in **tool visibility** and **prompt injection**, not binary availability. Condition A has wg on PATH but its bundle excludes all `wg_*` tools, so the agent never sees them in its tool surface.

---

## 2. Environment Variables and Their Meanings

The coordinator sets these env vars when spawning agents (`src/commands/spawn/execution.rs:547-565`):

| Variable | Source | Purpose |
|----------|--------|---------|
| `WG_TASK_ID` | Task being worked on | Agent knows which task to `wg log`, `wg done`, `wg artifact` |
| `WG_AGENT_ID` | Registry-assigned (e.g., `agent-7`) | Unique identity for message cursors, heartbeats, and agent tracking |
| `WG_EXECUTOR_TYPE` | Coordinator config | Tells agent which executor spawned it (`claude`, `amplifier`, `native`) |
| `WG_MODEL` | Resolved model string | The model the agent is running as; set only when explicitly resolved |
| `WG_TASK_TIMEOUT_SECS` | Coordinator config | Time budget for the task; used by `StateInjector` to inject time-pressure warnings |
| `WG_SPAWN_EPOCH` | `SystemTime::now()` at spawn | Combined with timeout to calculate elapsed/remaining time |
| `WG_USER` | `current_user()` | Human identity for audit trails |
| `WG_WORKTREE_PATH` | Worktree isolation | Path to the agent's isolated git worktree (when worktree isolation is active) |
| `WG_BRANCH` | Worktree isolation | Branch name for the agent's worktree |
| `WG_PROJECT_ROOT` | Worktree isolation | Root of the shared project |
| `WG_DIR` | Not set by spawn directly | Used by `wg --dir` flag; agents typically inherit from CWD |

### In-Process Tool Env Var Usage

The wg tools in `src/executor/native/tools/wg.rs` use `WG_TASK_ID` for default parent-task inference:
```rust
let Ok(current_task_id) = std::env::var("WG_TASK_ID") else {
    return vec![];
};
```
When an agent calls `wg_add` without `--after`, the tool automatically adds an `--after $WG_TASK_ID` dependency, creating implicit parent-child relationships.

---

## 3. Assessment of `wg quickstart` as Bootstrap Prompt

`wg quickstart` outputs ~560 lines covering:
- Getting started (init, setup, agency init, service start)
- **Skill & bundle setup section** — explicitly warns that agents need the skill/bundle installed
- Agency setup with role/tradeoff/agent creation
- Core commands (add, show, list, ready, done, fail, log, artifact)
- Service operations (start, stop, pause, resume, freeze, thaw)
- Monitoring (status, agents, watch, tui, viz)
- Advanced features (functions, replay, federation, analysis)

### Verdict: Too Long for Direct Prompt Injection

At ~560 lines, `wg quickstart` is a **reference document**, not a bootstrap prompt. It would consume 4-8K tokens of context window. However, it contains two pieces useful for bootstrap:

1. **The "SKILL & BUNDLE SETUP" section** (lines ~35-55) — this is the diagnostic "why isn't this working" guide
2. **The core commands table** — essential for any agent using wg

For containerized agents, the tiered guide system (`src/commands/spawn/context.rs:644-664`) is the right approach — it adapts content to model context window size:
- **Essential tier** (8KB): Core commands, decomposition patterns, env vars
- **Core tier** (16KB): + communication, graph patterns
- **Full tier** (40KB): + agency system, advanced patterns

---

## 4. `wg nex --role` and Skill Injection

`src/commands/nex.rs:138-163` implements role-based skill injection:

```rust
let role_prompt_addendum = if let Some(role_name) = role {
    if is_coordinator {
        // Static coordinator prompt
    } else {
        load_agency_role(workgraph_dir, role_name)
    }
};
```

`load_agency_role()` (nex.rs:599-629) scans `.workgraph/agency/primitives/components/*.yaml` for YAML files whose `name` field matches the role name (case-insensitive substring). It returns the `content` field as a prompt addendum.

### How It Works
1. User runs `wg nex --role programmer`
2. System scans `agency/primitives/components/` for a YAML with `name: programmer` (or containing "programmer")
3. The `content` field is appended to the system prompt under `## Role`

### For `--role coordinator`
The coordinator role is special-cased: instead of looking up a YAML, it injects a static prompt and **keeps all wg mutation tools** (wg_add, wg_done, wg_fail, etc.) which are otherwise stripped for interactive sessions.

### Could This Be Used for wg Awareness?
Yes — `--role` is a viable injection point. You could create a component YAML named "wg-aware" with content teaching wg basics. The agent would get it via `wg nex --role wg-aware`. However, this only works for `wg nex` sessions, not for bare native executor spawns inside Docker containers (Harbor path).

---

## 5. Agency Primitives Components

The `.workgraph/agency/primitives/components/` directory contains **hundreds of YAML files** (content-hash-named). These are agency role components with skills, desired outcomes, and trade-off configurations.

There is **no specific "wg-aware" skill component** in the current store. The existing components are domain-specific (programmer, reviewer, architect, etc.) and assume wg awareness comes from the prompt/executor, not from the agency system.

### Gap Identified
A "wg-awareness" skill component could be useful as a composable building block — an agent assigned a role with this component would receive wg usage instructions in their prompt. This would be a new primitive type: infrastructure awareness rather than domain expertise.

---

## 6. Harbor Adapter: Per-Condition System Prompt Differences

### Condition A (no wg)
- System prompt: `"You are a skilled software engineer. Complete the task below."`
- Tools: `bash`, `read_file`, `write_file`, `edit_file`, `glob`, `grep` (NO wg tools)
- Bundle: Custom `implementer.toml` that explicitly excludes wg tools
- Agent has no knowledge of wg at all

### Condition B+ (wg tools)
- System prompt: Same base + `WG_QUICK_GUIDE` (for legacy LLM path) or tiered guide (for native path)
- Tools: Full tool set including `wg_show`, `wg_list`, `wg_add`, `wg_done`, `wg_fail`, `wg_log`, `wg_artifact`
- Agent sees tool names and descriptions but gets minimal usage guidance

### Condition C (skill injection)
- System prompt: Base + **full skill prompt** with decomposition heuristics, planning phase, tool patterns
- Key addition: "ALWAYS" mandate for `wg_log`, complexity-based decomposition heuristic
- Designed to answer: "Does teaching *when* to use wg tools improve performance?"

### Condition F (distilled context)
- System prompt: Base + `CONDITION_F_MEMORY` (architecture, conventions, essential commands, common pitfalls)
- Provides **project knowledge parity** with what Claude gets natively from CLAUDE.md and memory
- Targeted at open-weight models that don't have built-in project understanding

### Condition G (autopoietic)
- System prompt: `CONDITION_G_META_PROMPT` — architect-only instructions
- Tools: Read-only + wg tools (no write_file/edit_file)
- Agent **must** decompose via `wg add` — cannot implement directly
- Multi-agent: up to 8 parallel workers dispatched by coordinator

### Condition G-smart (smart fanout)
- System prompt: `CONDITION_G_SMART_META_PROMPT` — try-first, decompose-if-needed
- Tools: Full set (can implement directly)
- Agent triages complexity before deciding direct vs decomposition
- Multi-agent: up to 4 parallel workers

---

## 7. Minimal "wg Awareness" Package for Containerized Agents

Based on the analysis, a minimal wg awareness package consists of four layers:

### Layer 1: Binary + Init (required)
```bash
# Upload wg binary
upload wg to /usr/local/bin/wg
chmod +x /usr/local/bin/wg

# Initialize graph
cd $TRIAL_WORKDIR && wg init

# Write config
echo "$CONFIG_TOML" > .workgraph/config.toml
```
This is what the adapter already does for all conditions.

### Layer 2: Environment Variables (required)
```bash
export WG_TASK_ID="$task_id"
export WG_AGENT_ID="$agent_id"
export WG_EXECUTOR_TYPE="native"
export WG_MODEL="$model"
export WG_SPAWN_EPOCH="$(date +%s)"
export WG_TASK_TIMEOUT_SECS="$timeout"
```
These are set by the coordinator's spawn logic.

### Layer 3: Tool Surface (configurable)
One of:
- **Full**: All wg tools in the tool registry (condition B+)
- **Read-only**: `wg_show` + `wg_list` only (interactive nex sessions)
- **None**: Bundle excludes wg tools (condition A)

### Layer 4: Prompt Injection (configurable)
Tiered by model capability:
- **Essential** (8KB): Core commands, decomposition patterns, env vars — from `build_essential_guide()`
- **Skill injection**: Condition C-style heuristics for when/why to use wg
- **Full context**: Condition F-style distilled project knowledge
- **Meta-prompt**: Condition G-style architect/smart-fanout instructions

### Minimum Viable Package
For a containerized agent to effectively use wg:
1. **wg binary on PATH** — `/usr/local/bin/wg`
2. **`wg init` run** — creates `.workgraph/` directory
3. **`config.toml` written** — configures coordinator, model, context scope
4. **Env vars set** — `WG_TASK_ID`, `WG_AGENT_ID` at minimum
5. **Essential guide in prompt** — the tiered guide from `build_essential_guide()`
6. **wg tools in tool surface** — at least `wg_show`, `wg_list`, `wg_done`, `wg_log`, `wg_add`

---

## 8. What Harbor Specifically Needs vs What's Already Built In

### Already Built In (wg core)
- ✅ Binary upload mechanism (adapter handles this)
- ✅ `wg init` and config.toml generation
- ✅ In-process wg tools (`src/executor/native/tools/wg.rs`) — no subprocess overhead
- ✅ Tiered guide system for prompt injection (`src/commands/spawn/context.rs`)
- ✅ Env var injection at spawn time (`src/commands/spawn/execution.rs`)
- ✅ Mid-turn state injection (`src/executor/native/state_injection.rs`) — messages, graph changes, time budget
- ✅ Bundle system for tool filtering (`src/executor/native/bundle.rs`)
- ✅ Agency role/skill injection via `--role` (`src/commands/nex.rs`)
- ✅ `wg skill install` for Claude Code skill injection (`src/commands/skills.rs`)
- ✅ Context scopes (clean/task/graph/full) controlling prompt assembly depth

### Harbor-Specific Additions (adapter layer)
- 🔧 Per-condition bundle overrides (e.g., Condition A's no-wg bundle written into container)
- 🔧 Per-condition meta-prompts (G architect, G-smart fanout)
- 🔧 Model normalization (`openrouter:model` ↔ `openrouter/model`)
- 🔧 Trial isolation (unique `/var/tmp/tb-trial-*` directories per trial)
- 🔧 Artifact download from container for analysis
- 🔧 `OPENROUTER_API_KEY` propagation into the daemon process
- 🔧 Verify command lookup (`lookup_verify_cmd`) for test-gate injection

### Gaps for Harbor
1. **No CLAUDE.md discovery** — containerized agents don't get CLAUDE.md because it's not in the Docker container. The adapter could upload it.
2. **No `wg skill install` equivalent for Docker** — the skill is designed for Claude Code's `~/.claude/skills/` directory, which doesn't exist in Docker containers. For native executor agents in Docker, the guide comes from the tiered system instead.
3. **No agency component for wg awareness** — a "wg-aware" skill component in the primitives store would allow the agency system to compose wg awareness into any agent identity, rather than hardcoding it per-condition.
4. **Condition C skill prompt is not factored** — it's inlined in `condition-c-design.md` as a design doc, not extracted as a reusable component. The tiered guide system already provides this functionality more flexibly.

---

## 9. Summary: Discovery Flow

For an agent spawned inside a container:

```
1. Binary: adapter uploads wg → /usr/local/bin/wg
2. Init:   adapter runs "wg init" in trial directory
3. Config: adapter writes config.toml (model, executor, context_scope, agency)
4. Bundle: adapter writes custom bundle.toml if needed (tool filtering)
5. Task:   adapter runs "wg add" to create the root task
6. Start:  adapter runs "wg service start" → coordinator spawns agent
7. Agent:  coordinator spawns native-exec with env vars:
           WG_TASK_ID, WG_AGENT_ID, WG_EXECUTOR_TYPE, WG_MODEL, etc.
8. Prompt: agent receives tiered guide + task description + context
9. Tools:  agent has in-process wg tools (wg_show, wg_add, wg_done, etc.)
10. Loop:  agent works, state_injection provides live updates (messages, graph, time)
```

The agent "discovers" wg through three channels simultaneously:
- **Tools**: wg_* functions appear in its tool surface (unless excluded by bundle)
- **Prompt**: tiered guide teaches commands and patterns
- **Environment**: env vars provide task identity and context

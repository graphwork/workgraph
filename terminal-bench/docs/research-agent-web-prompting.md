# Research: Agent Web-Search Prompting and Multi-Agent Fan-Out for TB

**Task:** research-agent-web-search
**Date:** 2026-04-08
**Status:** Complete

---

## 1. Current Agent Tool Access — Web Search Availability

### 1.1 Claude Executor (exec_mode="full")

When a task uses the **Claude executor** in `full` mode (`src/commands/spawn/execution.rs:867-895`), the spawned process is a full Claude Code session with:

- `--print --verbose --output-format stream-json --dangerously-skip-permissions`
- `--disallowedTools Agent` (only the Agent tool is blocked)
- No `--allowedTools` restriction

**Result:** WebSearch and WebFetch are **available as deferred tools** in the Claude Code environment. They are part of Claude Code's built-in tool set and are not blocked by the disallowedTools list. However, **no prompt text mentions their existence**, so whether agents use them depends entirely on the model's own initiative.

### 1.2 Claude Executor (exec_mode="light")

Light mode (`src/commands/spawn/execution.rs:834-865`) explicitly whitelists:

```
--allowedTools "Bash(wg:*),Read,Glob,Grep,WebFetch,WebSearch"
```

**Result:** WebFetch and WebSearch are **explicitly available and whitelisted**. This is the only exec_mode that names them specifically.

### 1.3 Native Executor (used by TB trials)

The native executor (`wg native-exec`) has its own tool system (`src/executor/native/tools/mod.rs:211-225`). `ToolRegistry::default_all()` registers:

- **File tools:** `read_file`, `write_file`, `edit_file`, `glob`, `grep` (from `file::register_file_tools`)
- **Bash tool:** `bash` (from `bash::register_bash_tool`)
- **WG tools:** `wg_show`, `wg_list`, `wg_add`, `wg_done`, `wg_fail`, `wg_log`, `wg_artifact` (from `wg::register_wg_tools`)

**No web search or web fetch tools are registered.** The native executor has no `web_search` or `web_fetch` tool implementation.

### 1.4 TB Condition Configs (`terminal-bench/wg/adapter.py:84-135`)

All TB conditions (A through G) use `exec_mode: "full"` with the **native** executor:

| Condition | exec_mode | Executor | Web Tools Available? |
|-----------|-----------|----------|---------------------|
| A | full | native | **No** — native executor lacks web tools |
| B | full | native | **No** |
| C | full | native | **No** |
| D | full | native | **No** |
| E | full | native | **No** |
| F | full | native | **No** |
| G | full | native | **No** |

**Key finding:** TB uses the native executor (not Claude CLI), so agents **cannot search the web** even though the exec_mode is "full." The native executor's tool registry simply doesn't include web tools.

### 1.5 Legacy Adapter (deprecated)

The old LiteLLM-based adapter (`adapter.py`) did include `web_search` and `web_fetch` as custom tool schemas (lines 211-257), backed by `duckduckgo_search` and `httpx`/`trafilatura` on the host. See `AUDIT-adapter-bypass-points.md` line 45. This capability was **lost** when TB moved to the native executor.

---

## 2. Current Prompting Analysis — Does It Mention Web Research?

### 2.1 Prompt Assembly Pipeline

The prompt is built by `build_prompt()` (`src/service/executor.rs:675-821`). The sections are:

1. System Awareness preamble (full scope only)
2. Skills preamble
3. Task assignment header ("You are an AI agent...")
4. Agent identity (role/tradeoff)
5. Task details (title, description)
6. Pattern keywords glossary
7. Verification criteria
8. Discovered test files
9. Context from dependencies
10. Required Workflow (7-step mandatory workflow: log, validate, commit, done)
11. Git Hygiene
12. Message Polling
13. Ethos ("The Graph is Alive")
14. Autopoietic Guidance / Decomposition
15. Graph Patterns
16. Reusable Functions
17. Critical WG CLI section
18. Project description / graph summary (graph+ scope)
19. CLAUDE.md content (full scope)

**None of these sections mention web search, WebFetch, WebSearch, internet access, or looking up documentation/solutions online.**

### 2.2 CLAUDE.md

The project `CLAUDE.md` mentions "research" only in the context of workgraph tasks ("create a research task — don't investigate yourself"). No mention of web capabilities.

### 2.3 TB Task Descriptions

Examined all 18 TB task instruction files (`terminal-bench/tasks/`). None mention:
- Searching for solutions
- Looking up documentation
- Using web resources
- Consulting external references

Tasks are self-contained problem descriptions (e.g., "Write a bash script...", "Fix the bugs in...").

### 2.4 Condition G Meta-Prompt

Condition G's task description tells the seed agent to build a workgraph and delegate, but does not mention researching solutions online.

---

## 3. Gap Analysis

### Gap 1: Native executor has no web tools (CRITICAL for TB)

The native executor (`wg native-exec`) does not implement web_search or web_fetch tools. Since all TB conditions use the native executor, agents **cannot** search the web even if they wanted to. The bash tool could theoretically be used for `curl` commands, but this requires the agent to independently decide to do so, and TB Docker containers may not have curl installed.

### Gap 2: No prompt mentions web capabilities (MODERATE)

Even for Claude CLI agents (non-TB, normal workgraph usage), the prompt never mentions that web search is available. Agents may discover WebSearch/WebFetch through Claude Code's deferred tool system, but this depends on the model's awareness and initiative. There is no explicit encouragement to search for existing solutions before implementing from scratch.

### Gap 3: TB task descriptions are fully self-contained (LOW for current tasks)

Current TB tasks are implementation tasks (write code, fix bugs, configure systems). They don't require external research — all necessary information is in the task description. However, harder tasks (e.g., COBOL modernization, Cython extensions, constraint scheduling) could benefit from agents looking up documentation, syntax, or library APIs.

### Gap 4: No research-first pattern in the workflow (MODERATE)

The Required Workflow section (steps 1-7: log, validate, commit, done) doesn't include a "research phase" step. There's no encouragement to understand the problem domain before coding. The Ethos section encourages decomposition but not research.

### Gap 5: Condition G has fan-out but no research intent (MODERATE)

Condition G (autopoietic, `max_agents=8`) supports multi-agent fan-out. However, the design intent is parallel task decomposition (split implementation work), not parallel research. There's no pattern for "one agent researches, another implements, a third verifies."

---

## 4. Concrete Recommendations

### 4.1 For TB Task Descriptions

**Do NOT add "search the web" instructions to current TB tasks.** Reason: The native executor doesn't have web tools, so the instruction would be misleading. Adding web tools to the native executor is a separate, larger change.

**Alternative (immediate):** For tasks requiring specialized knowledge (COBOL, Cython, constraint programming), add a "Hints" section to the task description with key documentation pointers or library names. This gives the agent the information that web search would provide, without requiring web tool infrastructure.

Example:
```
## Hints
- Cython build: requires setup.py with Extension() and cythonize()
- See: https://cython.readthedocs.io/en/latest/src/quickstart/build.html
```

### 4.2 For Native Executor Web Tools (Implementation Task)

Add `web_search` and `web_fetch` tools to the native executor tool registry. This requires:

1. **New tool module:** `src/executor/native/tools/web.rs`
2. **web_search:** Use DuckDuckGo's HTML API (no API key required) or a configurable search provider
3. **web_fetch:** HTTP GET with content extraction (similar to what the old adapter did with trafilatura)
4. **Registration:** Add `web::register_web_tools(&mut registry)` to `ToolRegistry::default_all()`
5. **Bundle filtering:** The research bundle should include `web_search` and `web_fetch`; the bare bundle should not

Estimated scope: ~200-400 lines of Rust code.

### 4.3 For Prompt Injection (Quick Win)

Add a short section to the `REQUIRED_WORKFLOW_SECTION` or a new section at the "task+" scope level:

```
## Available Capabilities

You have access to web tools for research:
- Use `web_search` to find documentation, examples, and existing solutions
- Use `web_fetch` to read web pages and documentation

When facing unfamiliar technologies or APIs, search for documentation before implementing.
```

This should only be injected when web tools are actually available (guard on tool registry contents or exec_mode).

### 4.4 For Multi-Agent Fan-Out Research Pattern

**Proposed pattern for Condition G (autopoietic):**

When the seed agent receives a task requiring specialized knowledge, it should create:

```
wg add "Research: find approaches for X" --exec-mode light \
  --verify "test -f /tmp/research-notes.md"

wg add "Implement: build X using research findings" \
  --after research-find-approaches-for-x \
  --verify "<original verify command>"
```

This is a **pipeline pattern** (research → implement), not fan-out. True fan-out for research would be:

```
wg add "Research: approach A for X" --exec-mode light
wg add "Research: approach B for X" --exec-mode light
wg add "Design: choose best approach" --after research-approach-a,research-approach-b
wg add "Implement: build X" --after design-choose-best-approach
```

**Problem:** This pattern is only useful when web tools exist in the native executor. Without them, "research" agents can only read local files.

### 4.5 For the Coordinator Prompt

The coordinator prompt (injected into the coordinator agent that dispatches work) should include:

```
When creating tasks for complex or unfamiliar problems, consider adding a research
subtask first (--exec-mode light) that investigates approaches before the main
implementation task begins.
```

This teaches the coordinator to proactively create research sub-tasks.

### 4.6 Amplifier Executor

The amplifier executor (`src/commands/spawn/execution.rs:896-926`) supports multi-agent delegation natively. It's not currently used for TB. If Condition G were run with the amplifier executor, it could delegate research sub-tasks without relying on the seed agent to create wg tasks manually. However, the amplifier also doesn't have web tools unless the underlying Claude Code session provides them.

---

## 5. Summary Table

| Question | Finding |
|----------|---------|
| Can TB agents search the web? | **No** — native executor has no web tools |
| Do prompts mention web search? | **No** — nowhere in the prompt pipeline |
| Is web search available for Claude executor agents? | **Yes** — WebSearch/WebFetch are deferred tools in Claude Code, but not mentioned in prompts |
| Do TB tasks need web search? | **Maybe** — current tasks are self-contained, but harder tasks could benefit |
| Is multi-agent fan-out available? | **Yes** — Condition G has `max_agents=8`, but fan-out is for implementation, not research |
| What's the quickest win? | Add hints to hard task descriptions (no code changes needed) |
| What's the right fix? | Add web tools to native executor, then add prompt guidance |

---

## 6. Recommended Implementation Priority

1. **P0 (now):** Add domain-specific hints to hard TB task descriptions
2. **P1 (next sprint):** Implement `web_search` and `web_fetch` in native executor tool registry
3. **P1 (next sprint):** Add prompt section about web tool availability (conditional on tools being present)
4. **P2 (later):** Add research-first pattern to coordinator prompt and Condition G meta-prompt
5. **P3 (experimental):** Test amplifier executor for TB to enable native delegation

---

## Files Referenced

| File | Relevance |
|------|-----------|
| `src/commands/spawn/execution.rs:867-895` | Claude executor "full" mode — no tool restrictions except Agent |
| `src/commands/spawn/execution.rs:834-865` | Claude executor "light" mode — explicitly whitelists WebFetch/WebSearch |
| `src/executor/native/tools/mod.rs:211-225` | Native executor tool registry — **no web tools** |
| `src/executor/native/bundle.rs:108-117` | Implementer bundle — wildcard tools, but only within native registry |
| `src/service/executor.rs:675-821` | `build_prompt()` — no web-related sections |
| `src/config.rs:796-816` | ExecMode enum — light tier mentions WebFetch/WebSearch in docs |
| `terminal-bench/wg/adapter.py:84-135` | Condition configs — all use exec_mode="full" with native executor |
| `terminal-bench/wg/AUDIT-adapter-bypass-points.md:45` | Old adapter had web_search/web_fetch tools |
| `CLAUDE.md` | Project instructions — no web capabilities mentioned |

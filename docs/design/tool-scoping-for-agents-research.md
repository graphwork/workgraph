# Tool Scoping for Native-Executor Agents (Research)

**Status:** research notes. Initial incident triage + refined design from subsequent discussion.
**Date:** 2026-04-16
**Context:** session with Erik on worktree sacredness, system prompt review, and an incident where a native-executor agent clobbered source files in the main tree.

This document originally captured design ideas that came up while investigating why a dispatched agent (qwen3-coder-30b on the `flip-web-search` task) hallucinated a replacement for the entire `src/executor/native/tools/web_search.rs` file and wrote it to the main tree despite being launched in a worktree. A later discussion with Erik refined the categorization — the revised scope table appears first below; the original research notes follow for context.

---

## Revised design (from 2026-04-16 follow-up)

The original table grouped `.assign-*` / `.evaluate-*` / `.place-*` / `.compact-*` together. That's wrong — **evaluate is fundamentally different** from the others and needs full agent capabilities.

### Final categorization

| Task type | Tool scope | Rationale |
|---|---|---|
| **`.flip-*`** | **nothing at all** — pure in-context reasoning | Fidelity check against latent intent. Sees task + artifacts in its prompt, returns a score. No tools needed. Failure mode shrinks to "wrong score" (recoverable via re-eval) instead of "hallucinated code on disk." |
| **`.assign-*`** | `wg` read-only (`list`, `show`, `context`, `status`, `ready`) — **no FS, no shell** | Job is to pick an agent for a task. Only needs to see the graph. Never needs to touch code. |
| **`.place-*`** | `wg` read-only (same subset as `.assign-*`) | Decides dependency edges / graph structure. Observability only. |
| **`.compact-*`** | read access to `.workgraph/context.md` + write to that one file | Narrow by design: reads distilled graph state, writes a new summary. Not full FS access. |
| **`.evaluate-*`** | **full agent — all tools including `wg` mutation** | *This is the escalation/repair path.* Evaluators may need to `wg fail` a broken task, open a triage task with `wg add`, or launch a repair agent. Gating this tool set would break the automatic-repair loop. |
| **Regular tasks** (`full`, `shell` exec modes) | full, with `write_file` and `edit_file` cwd-sandboxed | Where real implementation happens. Sandbox landed in commit `699376da`. |

### Why evaluate is promoted to full agent

An evaluator runs *after* a task completes. Its output is a judgment: "this did/didn't satisfy the intent." If the answer is "didn't," the evaluator may need to:

- Mark the task failed (`wg fail`)
- Decompose the failure into a follow-up task (`wg add "Investigate why X didn't work"`)
- Launch a repair agent (`wg add --after failed-task "Fix Y"`)
- Log context that downstream agents need (`wg log`)
- In extreme cases, investigate the repo to understand what went wrong

That's the entire set of things a regular full-agent can do. Restricting the evaluator defeats the "auto-heal" purpose — it becomes a passive critic that can't act on its own judgment. The safety argument ("don't give critic tools") is outweighed by the operational one ("critic is the only loop that can repair").

### Why assign/place stay read-only

Their output is a structured decision: "pick agent X, with exec_mode Y, context_scope Z, and these placement edges." That decision is consumed by the coordinator's IPC layer and applied to the graph — the assigner itself doesn't need to write anything. Read-only wg is sufficient.

### Why flip is *nothing*

Fidelity checks are pure reasoning. The task description + artifacts are in the prompt; the output is a number. Tools would only introduce attractors for unrelated behavior. Minimalism here is a feature.

### Enforcement path

Cleanest: tool-allowlist per task-type at registration time. When spawning an agent for `.flip-foo`, build a `ToolRegistry` with no tools; for `.assign-foo`, register only the wg-read subset; for `.evaluate-foo`, register everything (same as regular tasks). Tool names absent from the registry are rejected by the tool-call protocol — no runtime guard needed.

Two tools need finer-grained handling:

- **`wg` tool**: currently exposes all wg subcommands. For assign/place/compact we want a read-only subset. Either (a) introduce a second tool `wg_read` with only read-side commands, or (b) give the existing `wg` tool a mode parameter set at registration time.
- **`write_file`**: already cwd-sandboxed (commit `699376da`). For `.compact-*` we additionally want to restrict writes to `.workgraph/context.md` specifically. Can layer on a "write-whitelist" check inside the tool.

### Implementation shape

1. In `src/executor/native/tools/mod.rs`, the registry builder takes a task-type or exec-mode parameter.
2. Based on that parameter, it conditionally registers subsets.
3. `src/commands/spawn/execution.rs` plumbs the task-type (via task-id prefix inspection or task metadata) to the registry builder.
4. No changes to the agent loop itself — the loop already only calls registered tools.

Rough size: ~100-150 lines including a new `ToolRegistry::for_task_type()` constructor and the task-id-prefix → allowlist mapping.

---

## Original research notes (context preserved)

### The incident that motivated this

- A workgraph task `flip-web-search` (a regular `exec_mode=full` task — NOT a `.flip-*` meta task) was dispatched by the coordinator daemon.
- Three zombie `wg native-exec` processes were left running after `wg kill` was called (`wg kill` does not tree-kill child processes).
- One of them wrote a hallucinated replacement for `web_search.rs` directly into the main tree, destroying ~1700 lines of real code (all search backends, rate limiting, circuit breakers, etc.). Recovery was via `git checkout HEAD --`.

### Concrete findings (all now addressed)

1. `wg kill` does not tree-kill. Orphaned child processes keep writing. — **Fixed**, commit `699376da`.
2. `write_file` has no cwd sandbox — the model can pass absolute paths and escape the worktree entirely. — **Fixed**, commit `699376da`.
3. qwen3-coder-30b on a surgical-refactor task hallucinates wholesale rewrites instead of applying targeted edits. — **Known model limitation;** mitigated by (1) + (2) above and by not dispatching this model for code refactors.
4. The coordinator's agency binding pinned executor=native even when `[agent].executor=claude` was set in config. — **Explained:** `provider_to_native_provider` correctly overrides to native when the model is non-Anthropic; there was no bug, just a missing path when the user wanted Claude for orchestration.
5. A ghost agent (agent-16844) was spawned after the service was stopped — unknown cause. — **Root-caused + fixed** in commit `3294bcd8`. The daemon loop's coordinator-tick phase could fire after an IPC Shutdown set `running = false` but before the loop's while-condition re-checked.

### Original open questions (now answered)

1. *Do `.assign-*` / `.evaluate-*` actually need `bash` or `write_file` today?* — After discussion: assign does not, evaluate does. See revised categorization above.
2. *What is the actual prompt/payload passed to an `.assign-*` or `.evaluate-*` agent today?* — Assignment prompt inspected at `src/commands/service/assignment.rs:131`. Performance scores (`avg_score`, `task_count`) are rendered into the agent catalog — the LLM assigner sees them and is instructed to prefer higher-scoring agents.
3. *Is the structured-result path actually used?* — Yes, for assignment; the LLM returns JSON that the coordinator applies to the graph.
4. *How does `.compact-*` operate?* — Reads graph state, writes `.workgraph/context.md`. Narrow scope per revised table.
5. *Does the agency system have existing assumptions about tool sets?* — Roles have `default_exec_mode` (bare/light/full/shell). The exec-mode system is a partial answer but doesn't distinguish task-type-category (meta vs. regular).

---

## Related: bwrap sandboxing (deferred — separate design doc)

An earlier discussion covered using `bwrap` to provide kernel-enforced read-only binds on the source tree for meta tasks, which would make the "cannot write" property a kernel guarantee rather than a tool-registration convention. That is out of scope for this document and tracked separately by the user. Declarative tool-scoping (this doc) is portable and simple; bwrap is Linux-only but gives hard guarantees. Both layers can coexist — tool-scoping is the application-level boundary, bwrap is the OS-level backstop.

---

## Recommendation

The revised categorization is stable enough to implement. The big questions from the original doc (what do evaluate/assign need? does assign use write?) got answered through discussion and code inspection. Remaining work:

- **Do soon:** implement the tool-allowlist-per-task-type path. ~100-150 lines.
- **Consider as a follow-up:** a read-only `wg` tool variant vs. a mode parameter on the existing `wg` tool. Design call — either works.
- **Done (separate commits):** `write_file` cwd sandbox, `wg kill` tree-kill, ghost-agent fix, worktree sacredness, agent orphan reaping, oai-compat rename, disk cache fileification, SearXNG, Google News URL resolution, deep_research tool.
- **Out of scope here:** bwrap, model selection, agent self-dispatch policy.

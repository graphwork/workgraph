# Tool Scoping for Native-Executor Agents (Research)

**Status:** research notes, not a plan of action.
**Date:** 2026-04-16
**Context:** session with Erik on worktree sacredness, system prompt review, and an incident where a native-executor agent clobbered source files in the main tree.

This document captures a set of design ideas that came up while investigating why a dispatched agent (qwen3-coder-30b on the `flip-web-search` task) hallucinated a replacement for the entire `src/executor/native/tools/web_search.rs` file and wrote it to the main tree despite being launched in a worktree. The ideas may be worth pursuing; they may also be over-engineered for the actual problems we have. They are recorded here for future evaluation, not adoption.

---

## The incident that motivated this

- A workgraph task `flip-web-search` (a regular `exec_mode=full` task — NOT a `.flip-*` meta task) was dispatched by the coordinator daemon.
- Three zombie `wg native-exec` processes were left running after `wg kill` was called (`wg kill` does not tree-kill child processes).
- One of them wrote a hallucinated replacement for `web_search.rs` directly into the main tree, destroying ~1700 lines of real code (all search backends, rate limiting, circuit breakers, etc.). Recovery was via `git checkout HEAD --`.

Concretely-actionable findings (independent of the ideas below):

1. `wg kill` does not tree-kill. Orphaned child processes keep writing.
2. `write_file` has no cwd sandbox — the model can pass absolute paths and escape the worktree entirely.
3. qwen3-coder-30b on a surgical-refactor task hallucinates wholesale rewrites instead of applying targeted edits.
4. The coordinator's agency binding pinned executor=native even when `[agent].executor=claude` was set in config.
5. A ghost agent (agent-16844) was spawned after the service was stopped — unknown cause.

The research ideas below stem from (2) and the broader observation that the native executor currently exposes the same tool set to every task regardless of what that task actually needs to do.

---

## Research idea: task-type-gated tool registration

### Claim

The native executor could vary which tools it registers based on the task being run. Not every task needs every tool. Fewer tools = fewer attractors = fewer ways for a model to do something surprising.

### Proposed categorization

| Task type | Read FS | Write FS | Run `wg` | Shell | Notes |
|---|---|---|---|---|---|
| `.flip-*` | ❌ | ❌ | ❌ | ❌ | Pure judgment. Sees task + artifacts in context, returns fidelity score. If the only way to "do" is reason in text, the worst failure is "wrong score" — not "hallucinated code on disk." |
| `.assign-*`, `.evaluate-*`, `.place-*`, `.compact-*` | ✅ | ❌ | read-only subset | ❓ | Need observability to reason about task state and artifacts. Output is a structured decision returned to the coordinator, not a filesystem write. |
| Regular (`full`, `shell`) | ✅ | ✅ (cwd-sandboxed) | ✅ | ✅ | Where real implementation work happens. Still benefits from `write_file` being cwd-constrained. |

### Enforcement

Cleanest: **don't register the write tools at all** for restricted task types. Tool names not in the registry can't be invoked — the tool-call protocol rejects unknown names. No runtime guard needed.

### Open questions

These are the reasons this is "research" and not "plan":

1. **Do `.assign-*` / `.evaluate-*` actually need `bash` or `write_file` today?** The user's honest read: *"It's not clear that they don't and that they haven't been using it sometimes."* If these tasks are, in practice, shelling out or writing scratch files as part of how they work, removing those tools is a behavioral regression dressed as a security fix. Need to audit current usage before gating.

2. **What is the actual prompt/payload passed to an `.assign-*` or `.evaluate-*` agent today?** Before proposing what tools they should see, we need to see what they do see and what they do with it. A transcript of a few recent `.assign-*` runs would settle this.

3. **Is the structured-result path (graph mutation via coordinator IPC) actually used, or do these tasks mutate the graph directly via `wg` commands?** If it's the latter, then "read-only `wg` subset" is actually behaviorally incomplete.

4. **How does the `.compact-*` task actually operate?** It's meant to be the coordinator's own introspection cycle. Might need write access to `.workgraph/context.md` or similar — so "read-only" is probably wrong for this one.

5. **Does the agency system itself have assumptions about what tools are available to meta tasks?** Role definitions have `default_exec_mode`, which already hints at a tool-tier model. There may already be a way to do this without adding a new whitelist layer.

### Risks of pursuing this prematurely

- Restricting meta-task tools could silently break agency evaluation/assignment in ways that only surface under production load.
- Adds another axis of configuration complexity (per-task-type tool sets) when the simpler fix (cwd-sandbox `write_file`) covers most of the real attack surface.
- The actual incident was caused by a regular task, not a meta task. So this is tangential to the bug, not a direct response.

### Separable work that is worth doing regardless

The `write_file` cwd sandbox is independently valuable and applies to ALL tasks (regular and meta). Scope: inside `write_file`'s `execute()`, reject paths that (a) are absolute and outside cwd, or (b) contain `..` components that would escape cwd after canonicalization. This is ~15 lines and no new config axis. Recommend landing this regardless of how the broader scoping discussion resolves.

The `wg kill` tree-kill fix is also independently valuable. Scope: when killing an agent PID, also send the signal to its process group (or walk the process tree). This is another ~15 lines.

---

## Related: bwrap sandboxing (deferred — see separate design doc)

An earlier discussion covered using `bwrap` to provide kernel-enforced read-only binds on the source tree for meta tasks, which would make the "cannot write" property a kernel guarantee rather than a tool-registration convention. That is out of scope for this document and tracked separately by the user. The short version: declarative tool-scoping (this doc) is portable and simple; bwrap is Linux-only but gives hard guarantees. Both layers can coexist.

---

## Recommendation

- **Do soon:** `write_file` cwd sandbox. `wg kill` tree-kill. Both land-and-forget.
- **Research first, don't implement yet:** task-type tool scoping. Needs an audit of how `.assign-*` / `.evaluate-*` / `.place-*` / `.compact-*` actually behave in production before we decide what they "need."
- **Out of scope here:** bwrap, model selection, agent self-dispatch policy.

The broader point: we have intuitions about what meta tasks "should" be able to do, but we don't have data about what they do in practice. The right next step is an audit, not a restriction.

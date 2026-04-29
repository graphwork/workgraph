# Research Docs (Contributor Only)

> **This directory is for people hacking on workgraph itself.**
>
> Most documents here describe behaviors that are **already implemented**.
> Some are exploratory analyses for features that may or may not ship. You
> do not need to read anything here to USE workgraph. If you're a user
> looking for the agent / chat-agent role contract, run:
>
> ```
> wg agent-guide
> ```
>
> The bundled output of `wg agent-guide` — not these docs — is the
> authoritative source of agent behavior in any workgraph project.

## What lives here

Architecture analyses, protocol explorations, and prior-art surveys
produced as research tasks. Each document is a snapshot of investigation
at a moment in time — useful for tracing *how* a feature converged on its
current shape, not *how to use it*.

## Three documentation layers

Workgraph documentation is organized into three layers; this directory is
layer 3:

| Layer | Audience | Where it lives |
|-------|----------|----------------|
| **1. Universal role contract** | Every chat agent and worker agent in any project | Bundled in the `wg` binary; `wg agent-guide` |
| **2. Project-specific context** | Agents working on a specific project | `CLAUDE.md` / `AGENTS.md` at each project root |
| **3. Workgraph contributor docs** | People hacking on workgraph itself | `docs/designs/`, `docs/research/` (this directory) |

When in doubt: **users read `wg agent-guide` and project `CLAUDE.md`.
Contributors additionally read `docs/`.**

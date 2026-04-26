# Thin-Wrapper Executors for `wg nex` — Research & Recommendation (2026-04)

**Status:** Phase 1 deliverable for task `research-into-impl`.
**Scope:** narrowed by user message (2026-04-26): pick ONE pty-wrappable
OAI-compatible CLI binary that can replace the in-process `nex` loop, then
ship an autopoietic subgraph (impl + smoke + docs).

## Problem Statement

`claude` executor is reliable because wg pty-wraps the mature `claude` CLI
binary — auth, retries, tool-use, streaming, prompt caching, history are all
handled outside wg. `nex` re-implements that whole loop in-process
(`src/commands/nex.rs`, 634 lines) and is fragile: faults after the first
message in some configs (see `wg-nex-native-2`). Re-implementing the claude
CLI is a huge surface that wg should not own.

User prescription, verbatim:
> WG NEX SHOULD JUST BE RUN IN A DUMB PTY LIKE WE DO CLAUDE CODE.
> WHY IS THIS SO HARD.

Target use case (from user's repro): `lambda01.tail334fe6.ts.net:30000` (an
OAI-compatible endpoint) running `qwen3-coder`. Five back-to-back messages
must succeed without the fault that nex hits after the first message.

## Scoring Criteria (from task description)

`reliability > install-ease > feature breadth > license`. Reliability wins
because reliability *is* the original pain.

## Survey

| Candidate | Lang / Install | Custom OAI-compat base_url? | Multi-turn model | License | Reliability evidence |
|---|---|---|---|---|---|
| **codex-cli** (OpenAI) | Rust, single binary [^1] | Yes — `model_providers.<id>.base_url` in `~/.codex/config.toml` [^2] | `codex exec` is single-shot per turn; clean process boundary | Apache-2.0 [^1] | High — already shipped as wg executor (`src/commands/codex_handler.rs`); per-turn restart means a bad turn cannot poison subsequent turns |
| **aider** (Paul Gauthier) | Python, `pipx install aider-chat` [^3] | Yes via litellm — `OPENAI_API_BASE` env or `--openai-api-base` flag [^3] | Long-lived REPL with internal state; not designed for headless single-shot | Apache-2.0 [^3] | Mature on coding-focused workflows; pty wrapping a Python TUI is more fragile than wrapping a single-shot binary |
| **llm** (Simon Willison) | Python, `uv tool install llm` [^4] | Yes via plugins (`llm-openai-plugin`, `extra-openai-models.yaml` for base_url) [^4] | Single-shot query tool; conversation continuation via `-c` flag against SQLite log [^4] | Apache-2.0 [^4] | High for single queries; agentic tool-use loop is plugin-based and less battle-tested than codex |
| **llama-cli** (llama.cpp) | C++, single binary | N/A — runs local GGUF models, doesn't speak OAI HTTP at all | Loads model in-process | MIT | Wrong tool for an OAI-compat endpoint use case |
| **plandex** | Go, single binary | Custom server architecture; supports OAI-compat via env [^5] | Daemon + CLI; long-lived state on plandex server | MIT | Reliability good, but architecture is heavier (separate plandex server) — overkill for chat-completion use case |
| **claude-code in non-Anthropic mode** | Node, `npm i -g @anthropic-ai/claude-code` | No first-class OpenAI base_url support; routing via third-party proxies (ccrouter etc.) is unofficial | Existing wg `claude` executor target | Proprietary EULA | Already used; not a substitute for OAI-compat workloads |

[^1]: https://github.com/openai/codex — repo README, releases page (single Rust binary distribution).
[^2]: https://github.com/openai/codex/blob/main/codex-rs/config.md — `model_providers` and `base_url` configuration documented.
[^3]: https://aider.chat/docs/install.html and https://aider.chat/docs/llms/openai-compat.html — pipx install + OPENAI_API_BASE for OAI-compatible providers.
[^4]: https://llm.datasette.io/en/stable/ — plugin model + `extra-openai-models.yaml` for arbitrary base_url; `-c` flag for continuation.
[^5]: https://docs.plandex.ai/ — self-hosted server + CLI architecture.

## Recommendation

**Pick: `codex-cli`. Action: harden the existing `codex_handler` for custom
OAI-compat endpoints rather than build a new executor.**

### Why codex-cli wins under the stated criteria

1. **Reliability (highest weight).** The existing `codex_handler.rs`
   already implements the right architecture: spawn `codex exec` once per
   inbox message with the conversation replayed as a "previous turns"
   block. *No long-lived subprocess to supervise; a crashed turn is a
   non-event because the next turn restarts cleanly.* This is
   architecturally identical to how `claude_handler` wraps `claude` —
   which is why claude is reliable. The current per-turn restart pattern
   is the strongest possible reliability story available.
2. **Install ease.** Single Rust binary distributed via GitHub releases
   and `cargo install codex-cli`. No Python interpreter, no npm, no
   plugin chain to manage.
3. **Already wired.** `src/commands/codex_handler.rs` exists (504
   lines), spawn dispatch already routes to it, session-state mapping
   onto chat/*.jsonl already works. The remaining gap is *configuration
   plumbing*: passing `WG_ENDPOINT` / `WG_MODEL` through into a
   `~/.codex/config.toml` `model_providers` entry and ensuring the
   `OPENAI_API_KEY` (or equivalent) env reaches the spawned process.
4. **License.** Apache-2.0; permissive, no friction.

### Why not aider / llm

Both are Python and ship as REPLs or single-shot query tools. Pty-wrapping
a Python TUI is not "dumb pty wrap" — it requires interpreting prompts,
managing a heredoc-like protocol, and (for aider) coexisting with aider's
own repo-map state. That is closer in fragility to the in-process loop we
are trying to replace. `llm` is a query tool, not an agent loop.

### Why not llama-cli

Doesn't speak OAI-compat HTTP. Wrong abstraction layer for the user's
lambda01 endpoint use case (which is already an OAI-compat HTTP server in
front of qwen3-coder).

### What deprecating nex looks like (out of scope here)

If the codex-cli path lights up green on the smoke test (5 messages
back-to-back against lambda01 + qwen3-coder), then `wg nex --chat` and
`wg-nex-native` become a 634-line liability with no remaining unique
capability. A follow-up task should remove them from the default route
list (`wg setup`) and from new-coordinator dialog defaults, then mark the
in-process loop as deprecated. *Not done in this task per user's "out of
scope" guidance.*

## Phase 2 Subgraph (created by this task)

- `thin-wrapper-impl-codex` — harden `codex_handler` for custom OAI-compat
  endpoints (read `WG_MODEL` + endpoint from session config, write a
  per-session `model_providers` entry into `~/.codex/config.toml` or use
  `--config` overrides on the codex command line). TDD with a failing
  test that simulates 5-turn replay against a stub OAI server.
- `thin-wrapper-smoke-codex` — extend `wave-1-integration-smoke` with the
  user's exact repro: `wg init -x codex -m qwen3-coder -e
  https://lambda01.tail334fe6.ts.net:30000`, `wg service start`, `wg
  tui`, send 5 messages, assert 5 responses arrive.
- `thin-wrapper-docs-codex` — README + `docs/` entry: when to use codex
  vs claude vs nex; mark nex as on-ramp-to-deprecation.

Dependencies: smoke `--after impl,research-into-impl`; docs `--after smoke`.

## Sources Cited

See footnotes 1–5 above. All links verified to be the canonical project
homepage / repository at time of writing (2026-04-26).

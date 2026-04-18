# WGNEX executor improvements: post-unification design

Status: **proposed 2026-04-18**, superseding no prior doc. Sequel to
`nex-as-coordinator.md` (Phase 1-5, shipped 2026-04-18), which
collapsed the Claude-CLI coordinator into a single `wg nex` subprocess
backend. That work established that one executor runs everything;
this doc is about making that executor better.

## Principle

Ship three contained improvements to WGNEX, in order, before starting
anything bigger. Each phase is independently landable, independently
rollback-able, and measurably improves a known weakness. After all
three land, WGNEX has prompt-cache-aware Anthropic calls, real LLM
summarization on compaction, and MCP-tool plug-in capability — which
together close the three largest open gaps relative to every serious
peer in the executor space (OpenHands SDK, Amplifier, OpenCode,
Claude Agent SDK).

Deliberately **out of scope** for this doc: PyO3 bindings, the
permissions model, OpenTelemetry, explicit eval mode, middleware
hook registry. Those are the next-next-phase once this phase is
shipped and we have real usage data to inform them.

## Phase sequence and dependency

```
Phase 1: cache_control + tokenizer counts       (~1 day, tight)
   │
   ├─ unblocks → real cost measurements feed Phase 2 cost decision
   │
Phase 2: LLM-backed compaction                    (1 session)
   │
   ├─ unblocks → compaction pressure can absorb MCP's chattier
   │            tool outputs without degrading the session
   │
Phase 3: MCP client                                (1+ session, big)
```

Each phase commits cleanly on its own; each can be reverted in
isolation if a regression shows up. No phase blocks on the next;
the sequence is about risk and leverage, not hard dependencies.

---

## Phase 1 — cache_control on Anthropic direct + tokenizer counts

**One commit, two surgical changes that compound.**

### 1a. `cache_control` on outbound Anthropic requests

**Problem.** `src/executor/native/client.rs` *reads* `cache_creation_input_tokens`
and `cache_read_input_tokens` from Anthropic responses for cost
accounting, but never sets `cache_control` on outgoing content blocks.
Every coordinator turn re-uploads the system prompt (often 10–14k
tokens of composable coordinator prompt), the tool definitions
(~5–8k tokens), and the full message history at full input price.
Anthropic prompt caching is 90% cheaper on cache reads — this is
the single highest-ROI change in the file.

OpenRouter path (`openai_client.rs`) already sets a top-level
`cache_control` field that OpenRouter translates; direct-Anthropic
path does not.

**Design.**

Anthropic supports up to 4 `cache_control` breakpoints per request,
applied to specific content blocks. The optimal placement for a
long-running coordinator:

1. **System prompt** — stable across the whole session. Always cache.
2. **Tool definitions** — stable unless the registry changes mid-run
   (rare). Always cache.
3. **Oldest stable message prefix** — everything up to the
   most-recent compaction checkpoint. Cache when the prefix is
   ≥1024 tokens (below that, caching overhead exceeds the benefit).
4. **Latest pre-turn checkpoint** — the last message before the
   current turn. Cache so the next turn's prefix is fully cached.

TTL: default to Anthropic's 5-minute ephemeral cache. Add a
`--cache-ttl 1h` flag / config option later for longer sessions;
leave 5m as the default because the 1h cache has a minimum spend.

**Where.**

- `src/executor/native/client.rs`
  - Add `cache_control: Option<CacheControl>` to the serializable
    `ContentBlock` / request-body types (skip_serializing_if None
    so non-Anthropic providers don't see it).
  - In `build_messages_request`, after building the system + tools
    + messages, tag the four breakpoints. Gate on
    `self.provider_is_anthropic()` so OpenAI-compatible paths are
    unaffected.
- `src/executor/native/openai_client.rs` — no change; its existing
  OpenRouter cache_control field handles that path.

**Testing.**

- Unit: deterministic test that for a known system + tools +
  messages input, the request body has exactly 4 cache_control
  breakpoints in the expected positions.
- Live (gated, manual): run `wg nex --chat-id 0 --role coordinator`
  against claude-haiku-4-5 with a long system prompt, send two
  consecutive messages, inspect the response's
  `cache_creation_input_tokens` on turn 1 and `cache_read_input_tokens`
  on turn 2. Expect turn 2 to cache-read ≥80% of turn 1's input.

**Acceptance.** Live test shows cache-read tokens > 0 on turn 2.
Usage log reflects cost reduction.

### 1b. Tokenizer-aware token counts

**Problem.** `ContextBudget` in `src/executor/native/resume.rs` uses
`chars / 4` as its token estimate. That's a systematic ~20-25%
undercount for code (code has a worse tokens-per-char ratio than
English prose: curly braces, identifiers, punctuation). On a 32k
window, the compaction-pressure threshold therefore fires late —
by the time we decide "soft-pressure, compact at next boundary,"
the next turn has already blown the window.

**Design.**

Use the `tokenizers = "0.21"` crate (HuggingFace, pure-Rust, no
Python). Map model → tokenizer via a small registry:

| Model family prefix     | Tokenizer source             |
|-------------------------|------------------------------|
| `claude-`, `anthropic:` | cl100k_base (close approx.)  |
| `gpt-`, `openai:`       | cl100k_base                  |
| `qwen`                  | qwen bundled JSON            |
| `gemini-`, `google:`    | cl100k_base (rough)          |
| fallback                | cl100k_base                  |

Anthropic doesn't publish its tokenizer; cl100k_base gives counts
that are ~5% off — good enough for pressure detection. Perfect
counts aren't the point; replacing a 25% systematic undercount with
a 5% jitter is.

Tokenizer loads are expensive (~10-50ms, file I/O + deserialization).
Cache per-model at `AgentLoop` construction. If load fails, fall
back to the 4-chars-per-token heuristic with a single warn-log —
never panic, never break the session.

**Where.**

- `Cargo.toml` — `tokenizers = "0.21"`.
- New `src/executor/native/tokenizer.rs`: small module with
  `load_tokenizer_for(model) -> Result<Arc<Tokenizer>>`, backed by
  a `OnceLock`-protected per-model cache.
- `src/executor/native/resume.rs` — `ContextBudget::effective_tokens`
  calls the tokenizer for real counts. Keep the old
  `estimate_tokens_chars` helper as a labeled fallback.

**Testing.**

- Unit: loading claude, gpt, qwen tokenizers returns different
  counts for the same prose (sanity check that we're actually
  tokenizing, not just char-counting).
- Regression: for a fixed test corpus (5k of Rust source), assert
  the real count is in the expected range and the old heuristic
  undercounts by at least 15%.

**Acceptance.** Existing pressure-detection tests pass with real
tokenizer. New regression test demonstrates undercount correction.

### Phase 1 commit shape

One commit, title something like
`feat(nex): Anthropic cache_control + tokenizer-aware token counts`.
Both changes are self-contained and compose cleanly. Merging them
together makes the cost accounting sensible in one hop — cache_control
reduces the number of uncached tokens, tokenizer counts measure
accurately how many tokens we had.

---

## Phase 2 — LLM-backed compaction

**One commit. Closes the comment in `resume.rs:287-289` that has
been there since day one.**

### Problem

`summarize_messages` in `resume.rs:289` is a local heuristic
extractor: it walks the message list, grabs tool-call names and
the first 200 chars of text blocks, joins them. That's enough to
give the replay *something* post-compaction, but the resulting
summary is lossy — it doesn't preserve the *reasoning* or
*decisions* the agent made in the compacted prefix.

The doc comment explicitly says:

> For deeper summarization, the compaction entry type in the
> journal can be used by an external process.

There is no external process. The `JournalEntryKind::Compaction`
exists, replay handles it, but no one writes a good summary into it.

OpenHands' `LLMSummarizingCondenser` reports ~2× cost reduction
*and* higher task continuity post-compact. We want both.

### Design

On compaction:

1. Split messages at `split_point = len - KEEP_RECENT_MESSAGES`
   (unchanged).
2. Call the LLM with a dedicated summarization prompt over the
   older prefix. The summary uses a structured 9-section template
   already present elsewhere in the codebase (ported from Claude
   Code; see `session_summary` machinery) — work completed,
   decisions made, open questions, files touched, tools used,
   state to preserve, things to NOT re-do, next steps, caveats.
3. Write the returned text into a new `JournalEntryKind::Compaction`
   entry with fields `{ pre_tokens, post_tokens, summary_text,
   model_used, timestamp }`.
4. Inject the summary into the compacted message list as a user
   message tagged `[PRIOR SESSION SUMMARY]`, same way the local
   heuristic does today.

Model choice: use the session's configured model by default.
Add `config.native_executor.compactor_model` for projects that
want a cheaper dedicated summarizer (e.g. coordinator runs opus,
compactor uses haiku). Single LLM call, non-streaming, 2k token
cap on the summary itself.

Failure handling: if the LLM call fails (timeout, rate limit,
context overflow on the summary input), fall back to the local
heuristic and annotate the journal entry with
`fallback_reason: "llm_failed"`. Never let a failed summary
break the session.

Cost: typically 1-3 compaction events per long session. At
haiku rates on a 20k-token prefix, that's ~$0.02 per compaction.
Trivial.

### Where

- `src/executor/native/resume.rs` — add
  `async fn llm_summarize_messages(client, messages, config) -> Result<String>`.
  Replace `summarize_messages(older)` call with a try-LLM-first,
  fall-back-local pattern.
- `src/executor/native/journal.rs` — enrich
  `JournalEntryKind::Compaction` with `summary_text`, `model_used`,
  `fallback_reason` fields (backward-compatible: old entries
  without these fields still deserialize).
- `src/config.rs` — add `[native_executor] compactor_model`
  optional field.

### Testing

- Unit: mock client returning a canned summary; assert the
  Compaction entry contains it and the compacted message list
  begins with the summary.
- Unit: mock client returning an error; assert fallback runs and
  the entry has `fallback_reason` set.
- Integration (live, gated): drive a session to the soft-pressure
  threshold, confirm `Compaction` entry in the journal has
  `summary_text` of reasonable length (>500 chars, <4k chars) and
  that a subsequent turn's response shows the agent remembering
  the prior work.

### Acceptance

Journal's Compaction entries contain real LLM-generated summaries
in normal operation. Fall-back path verified by test. Usage log
shows the compactor's extra cost.

---

## Phase 3 — MCP client

**A proper sprint. One or more commits, scoped phases within.**

### Problem

WGNEX has no MCP support. Every serious peer (OpenCode, Goose,
Amplifier, Cline, Nanobot, Claude Desktop, Claude Agent SDK) has
it. The growing ecosystem of MCP servers — GitHub, Sentry, Linear,
Slack, Notion, filesystem, sqlite, browser-automation, sequential-
thinking, memory, fetch — is currently inaccessible to a WGNEX
agent. Our tool set in `executor/native/tools/` is hand-rolled,
so every new integration is a hand-written Rust tool. This is the
biggest strategic gap.

### Scope boundaries

Ship in three sub-phases so we don't dump 2000+ lines in one
commit:

- **3a. stdio transport, tools only.** JSON-RPC 2.0 over stdio,
  `tools/list` + `tools/call`, schema translation to Anthropic's
  tool format. Config-driven server launch. Tools appear
  indistinguishable from native tools to the agent.
- **3b. resources + prompts.** `resources/list` + `resources/read`
  as a first-class `mcp_read` tool. `prompts/list` + `prompts/get`
  surface as slash-commands (`/mcp:<server>:<prompt>`).
- **3c. SSE transport.** For MCP servers that don't fork (HTTP-
  based). Lower priority — most valuable servers today are stdio.

WebSocket, OAuth, and remote MCP discovery are not in this doc.

### Design — 3a

New module `src/executor/native/mcp/`:

```
mcp/
├── mod.rs          — pub API: McpManager, McpTool
├── transport.rs    — StdioTransport + trait Transport
├── client.rs       — JsonRpcClient (request/response/notification)
├── supervisor.rs   — server lifecycle: spawn, keepalive, restart
├── schema.rs       — MCP JSON Schema → Anthropic ToolDefinition
└── registry.rs     — McpToolRegistry: merges MCP tools into the
                     main ToolRegistry alongside native tools
```

Config surface in `.workgraph/config.toml`:

```toml
[[mcp.servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/workspace"]
enabled = true

[[mcp.servers]]
name = "sequential-thinking"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-sequential-thinking"]
env = { FOO = "bar" }
enabled = true
```

Tool namespacing: MCP tools appear in the registry as
`<server-name>__<tool-name>` (two underscores) so there's no
collision with native tools. The agent sees them in its tool
list like any other.

Lifecycle: one `McpManager` per `AgentLoop`, spawned during
`AgentLoop::with_tool_support`. Servers start in parallel on
spawn, log stderr to `<workgraph>/chat/<ref>/mcp-<server>.log`.
Rate-limited restart on crash (same policy as coordinator
subprocess: 3 restarts per 10 min). Shutdown sends `exit`
notification + SIGTERM + SIGKILL with grace periods.

Schema translation: MCP tool schema is JSON Schema Draft 7.
Anthropic's tool format is a subset. Translation handles `type`,
`properties`, `required`, `enum`, `description`, nested objects,
arrays. Unsupported constructs (`oneOf`, `allOf`, `$ref`) emit
a warning and fall through as `type: "string"` with the raw
schema in the description.

### Design — 3b

Resources: expose via a single built-in `mcp_read` tool taking
`server` + `uri`. The agent inspects `resources/list` output
(surfaced on demand via an `mcp_list_resources` tool) and calls
`mcp_read` to fetch content. This avoids N tools per N resources
while still making them reachable.

Prompts: surface as slash-commands inside `wg nex`'s REPL layer
(`handle_nex_slash_command`), mapped `/mcp:<server>:<prompt>`.
Autocomplete via the existing rustyline completer.

### Design — 3c

Deferred. Add `SseTransport` when a specific user need lands.

### Where (concrete file map)

**3a touches:**
- `Cargo.toml` — likely no new deps (hand-rolled JSON-RPC; reuse
  existing `serde_json` + `tokio::process`).
- `src/executor/native/mcp/*` — new module.
- `src/executor/native/mod.rs` — export.
- `src/executor/native/tools/mod.rs` — merge MCP tools into the
  registry.
- `src/executor/native/agent.rs` — `AgentLoop` holds an `McpManager`,
  shuts it down on session exit.
- `src/config.rs` — `McpConfig` struct with server definitions.
- `src/commands/nex.rs` — wire the config through; add
  `--no-mcp` flag to disable for debugging.

### Testing — 3a

- Unit: schema translation round-trip for a handful of
  real-world MCP tool schemas (filesystem, sequential-thinking,
  fetch). Golden files.
- Unit: JSON-RPC client happy path + error cases (malformed
  response, server crash mid-request, concurrent requests).
- Integration: spawn `@modelcontextprotocol/server-filesystem`,
  call `read_file` via the translated tool, assert contents.
  Requires npx in CI; gated behind a `mcp-integration-tests`
  feature flag.

### Acceptance — 3a

`wg nex --chat <ref>` with a config-enabled filesystem MCP
server exposes `filesystem__read_file` in the agent's tool list,
and the agent can successfully call it and use the result.

---

## What we are explicitly NOT doing in this phase

For future phases, not now:

- **PyO3 bindings.** Dismissed in Phase 0 review — MCP largely
  solves the ecosystem-access problem it was aiming at.
- **Permissions model.** Important for shared deployments but not
  for the current single-user-per-daemon story. Defer until a
  concrete multi-user use case lands.
- **OpenTelemetry.** Nice for long multi-agent runs but the
  journal already gives us per-session traceability. Defer.
- **Explicit eval mode.** Worth doing to validate the small-model
  claim, but should follow the executor improvements not precede
  them — no point benchmarking before the improvements are in.
- **Tool-output streaming.** Worth verifying behaves correctly on
  long bash / web_fetch, but fix as a bug rather than as a
  planned phase.
- **Middleware / hook registry.** Open SWE's pattern is appealing
  but `state_injection` covers 80% of the use case; resist
  abstracting further until a second concrete need appears.

## Acceptance of the whole design

Each phase ships independently. After Phase 3a (MCP stdio tools),
the executor has closed the three largest gaps vs. the peer set
and WGNEX is ready for broader distribution / external eyes.

**Total estimated scope:**
- Phase 1: ~200-300 LOC across 3-4 files. 1 day.
- Phase 2: ~200 LOC in 3 files. 1 session.
- Phase 3a: ~1000-1500 LOC in a new module. Multi-session.
- Phase 3b + 3c: additive, sized similarly to 3a.

## Rollback plan

- Phase 1: a single commit revert. Cache_control is additive;
  tokenizer is behind a fallback.
- Phase 2: revert the Compaction entry enrichment + the
  `llm_summarize_messages` call. The heuristic `summarize_messages`
  is kept.
- Phase 3: the `McpManager` is optional. `--no-mcp` disables, and
  the config's `[[mcp.servers]]` list defaults empty.

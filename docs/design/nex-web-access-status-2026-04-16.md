# Nex Web Access — Status & Next Steps (2026-04-16)

## What was built this session (32 commits to main)

### Web search pipeline (10 backends, parallel fan-out)
- Wikipedia OpenSearch, Google News RSS, HN Algolia, GitHub search, Stack Exchange, crates.io, arxiv, headless Chrome DDG, reqwest DDG HTML, Brave Search API
- All fire in parallel, results merged by URL, ranked by multi-source agreement
- Politeness: 10-min LRU cache (in-memory), per-backend rate limiters, 3-strike circuit breakers, Retry-After honor, descriptive User-Agent

### Browser emulation stack
- `rquest` with Chrome136 TLS/JA3/HTTP2 emulation as primary HTTP path for ALL web tools
- Headless Chrome via `chromiumoxide` as fallback (shared singleton, stealth flags, persistent `~/.wg-chrome-profile` for cookie persistence across sessions)
- Chrome process cleanup via SIGKILL in Drop impl
- `reqwest` switched to `rustls-tls` to avoid OpenSSL/BoringSSL symbol clash

### Research tool (`research(query, instruction)`)
- High-level primitive: search → fetch top 4 pages via Chrome → summarize each → merge into brief
- Google News RSS redirect resolution (follows opaque redirects to get actual article URLs)
- CAPTCHA detection + skip-and-try-next (tries up to 8 URLs, uses first 4 that return real content)
- Model resolution from config (not hardcoded Claude fallback)

### web_fetch improvements
- File artifact architecture: pages saved to `.workgraph/nex-sessions/fetched-pages/NNNNN-<slug>.md`
- Returns metadata + 20-line preview + bash hints + summarize guidance for large pages
- PDF support via `pdftotext` (Content-Type detection, binary handling)
- Two-tier: rquest primary, headless Chrome fallback
- `path_used` measurement stamps for stats aggregation

### Agent loop unification
- `run()` is now 3 lines — delegates to `run_interactive(autonomous=true)`
- Single canonical loop for both nex REPL and background task agents
- All reliability features (resume, compaction, state injection, heartbeat, summary extraction) work in both modes

### Display UX
- Three modes: default (one-line summary per tool), `-c` chatty (full output), `-v` verbose (+ diagnostics)
- Post-compaction context note injected into model's message array
- `/compact` slash command for manual compaction
- `/status` slash command showing context usage, message breakdown, compaction history, file paths

### Session infrastructure
- Journal wiring: every nex session produces `.journal.jsonl` (full replayable conversation)
- `--resume` flag loads most recent journal and continues from where you left off
- `--read-only` / `-r` safety mode (only safe tools)
- `--role <name>` loads agency skills from primitives; `--role coordinator` restores wg tools

### Grounding fixes
- Tool output channeler exempts `web_search` (the 2KB threshold was eating 8KB of real results, causing qwen3 to hallucinate)
- Plain text format with grounding rule preamble (kills hallucination for small models)
- Compact summary header as first line (for non-chatty display)

### Worktree safety
- ALL automatic worktree cleanup removed (3 paths killed)
- `wg worktree list` + `wg worktree archive <id> [--remove]`
- Worktrees are sacred — only removable via explicit user action
- `worktree_isolation = true` re-enabled in config

### Other fixes
- `wg_done` idempotent + explicit stop signal (prevents retry loops)
- wg mutation tools stripped from nex (wg_done/wg_add/wg_fail/wg_artifact)
- Auto-fetch system prompt updated to prefer `research` tool
- Current date/time in system prompt
- html5ever WARN spam suppressed
- Brave Search API key readable from config.toml or env var
- `profile = "anthropic"` removed from config

## Current state of web access

### What works well
- Google News RSS: reliable, real content, good for news + surprisingly broad coverage
- Wikipedia OpenSearch: rock-solid for factoid queries
- HN Algolia: great for dev/tech queries
- GitHub/Stack Exchange/crates.io: domain-specific, stable JSON APIs
- DDG via headless Chrome: works when Chrome profile has cookies
- PDF extraction via pdftotext
- Grounding: qwen3-coder-30b now cites real names from real search results (Vomero pizzeria test passed)

### What doesn't work well
- TripAdvisor, TheFork, LinkedIn: DataDome anti-bot blocks even headless Chrome
- PubMed, Nature: JS-rendered, need Chrome but often serve thin content
- DDG via rquest: intermittent (challenge pages when DDG rotates its detection)
- High-volume research: free backends rate-limit, cache is in-memory only

### Known limitations
- Research tool fetches 4 pages sequentially (could parallelize)
- Google News RSS redirect resolution adds latency per URL
- No persistent search cache across sessions
- No SearXNG integration yet
- No query decomposition for complex research questions

## IMMEDIATE NEXT STEPS (priority order)

### 1. Deep research mode (decompose → focused queries → synthesize)
**Why:** Reduces query count by 10-100x for complex research questions. Instead of the model firing 50 variations of the same query, a planning step decomposes into 5-10 focused sub-questions.

**Shape:**
```
deep_research(question="What is the full publication timeline of Vincenza Colonna's work on imprinting disorders?")
→ decompose into sub-questions:
  1. "Vincenza Colonna researcher affiliation"
  2. "Vincenza Colonna publications imprinting disorders PubMed"
  3. "DNA methylation imprinting disorders timeline key papers"
→ run research() on each
→ synthesize across all briefs
→ return comprehensive answer
```

**Implementation:** ~200 lines. Use the model itself to decompose (one LLM call with "break this research question into 3-5 specific searchable sub-questions"), then fan out `research()` on each.

### 2. Persistent disk cache for web search
**Why:** Eliminates repeat queries across sessions. Same query next week returns instantly. At research scale, this alone could cut 10K queries to 2K.

**Shape:** SQLite database at `~/.workgraph/web-cache.db` with columns `(query_normalized, response, backend, timestamp)`. TTL configurable (default 7 days for search results, 30 days for fetched pages). Replace the in-memory LRU in web_search.rs.

**Implementation:** ~150 lines. `rusqlite` crate (already battle-tested, single-file DB). Check cache before fan-out, write after successful search.

### 3. SearXNG integration
**Why:** Unlimited general search, self-hosted, no API costs, no rate limits. The right answer for high-volume research.

**Setup:** `docker run -d --name searxng -p 8888:8080 searxng/searxng` with a small `settings.yml` override to enable JSON format.

**Implementation:**
- New `Backend::SearXNG` variant in web_search.rs
- Gated on `WG_SEARXNG_URL` in config.toml or env var
- Hits `<url>/search?q=QUERY&format=json`
- Parses standard SearXNG JSON response
- ~80 lines for the backend
- Document the docker setup in a guide

**Config:**
```toml
[native_executor.web]
searxng_url = "http://localhost:8888"
```

### 4. r-indexed Common Crawl (long-term research project)
**Why:** The only path to "real general search with no external dependency." 150-400 GB on a single NVMe, stale (4-8 weeks) but broad.

**Status:** Design documented in `docs/design/todo-web-search-fixes.md`. Requires: PFP → BWT → r-index construction pipeline. All tools exist (bigbwt, pfp-thresholds, r-index, movi). Nobody has connected them end-to-end for CC-scale natural language. This is pangenomics-toolchain-shaped work.

**Not on the immediate roadmap** but worth tracking as a separate project.

## OTHER DEFERRED ITEMS

### Infrastructure bugs
- Chat compactor: re-enabled at interval=30, should work with native+qwen3 config but untested
- ArchiveCoordinator IPC broken pipe: handler looks correct, likely a timing issue. Test with TUI.
- Coordinator auto-creates tasks on empty graph: needs either user confirmation before creating or `auto_create = false` enforcement

### Architecture
- Research tool: parallelize page fetches (currently sequential)
- Research tool: add Brave as a fetch source (not just search — Brave has a "summarizer" endpoint)
- Session-level web stats aggregation (`wg nex-stats`)
- Role-driven tool loading beyond simple --role flag (deeper agency integration)

### Config
- `~/.workgraph/config.toml` as global config (resolver already supports it)
- Brave API key works from config or env var ✓
- SearXNG URL needs config support (field exists: `searxng_url`)

## FILES CHANGED THIS SESSION

Key files (most heavily modified):
- `src/executor/native/agent.rs` — unified loop, slash commands, compaction, journal
- `src/executor/native/tools/web_search.rs` — 10 backends, politeness, grounding format
- `src/executor/native/tools/web_fetch.rs` — file artifacts, PDF, rquest, Chrome fallback
- `src/executor/native/tools/research.rs` — NEW: high-level research primitive
- `src/executor/native/channel.rs` — NEVER_CHANNEL_TOOLS exemption
- `src/commands/nex.rs` — CLI flags, system prompt, role loading, resume
- `src/commands/service/mod.rs` — worktree cleanup removed
- `src/commands/service/triage.rs` — worktree cleanup removed
- `src/commands/service/worktree.rs` — referenced but not modified (cleanup was in callers)
- `src/commands/worktree_cmd.rs` — NEW: wg worktree list/archive
- `src/cli.rs` — new flags and subcommands
- `src/main.rs` — resolver, flag threading, env_logger filter
- `Cargo.toml` — rquest, rquest-util, lru, chromiumoxide deps; reqwest switched to rustls-tls

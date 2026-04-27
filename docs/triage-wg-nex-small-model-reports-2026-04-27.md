# Triage: 3 small-model bug reports from `wg nex` (Qwen3-Coder-30B-MoE)

**Date:** 2026-04-27
**Source:** Three reports filed by a Qwen3-Coder-30B-A3B mixture-of-experts
model running through `wg nex`. User context: *"these small models are useful
if they're going to get access to information, but it seemed as having trouble
doing that."*
**Source files** (moved into `docs/agent-reports/`):
- `tool_call_processing_bug_report.md`
- `ui_freeze_bug_report.md`
- `ecmwf_analysis_limitation.md`

This triage validated each report against the actual `src/executor/native/`
code, identifies real bugs vs. model misunderstandings, and proposes follow-up
work.

---

## TL;DR verdicts

| # | Report | Verdict | Effort to address |
|---|---|---|---|
| 1 | Tool call processing — no streaming during tool input emission | **Real bug, partial** — text deltas stream, tool-input deltas don't | medium |
| 2 | UI freeze during long `write_file` | **Same root cause as #1** | (covered by #1) |
| 3 | ECMWF / binary file / auth / SVG-PNG limitations | **Mostly model misunderstanding + one config doc gap** | small (docs + system prompt) |

Reports 1+2 are one investigation; treat them as a single bug.

---

## Report 1+2 — Tool-call streaming and UI "freeze"

**The model's claim:** "Users must finish typing the entire tool call before
any output appears." "Long-running operations freeze the UI until completion."
"No real-time metrics."

**What I verified in code:**

`wg nex` runs an `AgentLoop` in REPL mode. The provider client
(`src/executor/native/client.rs:289 messages_streaming_with_callback`) does
real SSE streaming: it parses each `content_block_delta` event off the wire and
forwards `TextDelta` content immediately to a `on_text` callback. In
interactive mode, the callback writes via `eprint!` to stderr and flushes
(`src/executor/native/agent.rs:1707`). So **plain assistant text streams
correctly** — the user sees it character-by-character.

The freeze the model is describing is real, but its diagnosis is wrong. The
SSE protocol carries tool calls as a separate delta type
(`InputJsonDelta { partial_json: String }`,
`src/executor/native/client.rs:184`). The streaming code accumulates these
into a per-block JSON buffer (`client.rs:688`) but does **not** invoke any
user-facing callback while doing so. There is no "tool input is being typed"
indicator. So when a small (slow) local model generates a `write_file` call
with 5,000 lines of content, the user sees:

1. Some assistant text (streams fine).
2. Long silence while the model emits a JSON-quoted string of file content,
   token by token, into the tool_use block.
3. The tool actually executes — `fs::write` is instant — and the result block
   appears all at once.

What the model misdiagnosed as "the UI waits for the complete response" is
actually "the model itself is taking a long time to emit the tool input, and
nothing reports that work to the user." The fix is **not** to stream tool
output during tool execution (`write_file` finishes in microseconds — there's
nothing to stream); it's to surface the *pre-execution* work the model is
doing.

The model also asked for tokens/second metrics. None are wired through to the
user. The data exists (response usage block), but it's only displayed at end
of session in `--verbose` mode (`nex.rs:441`). A live tok/s readout would be
straightforward to add to the spinner row.

**Verdict: real bug (partial — text streaming works, tool-input streaming
doesn't; tok/s is unimplemented).**

**Proposed fix scope (medium):**

- In `messages_streaming_with_callback`, also forward `InputJsonDelta` to a
  new `on_tool_input` callback. The agent's interactive sink should render
  this as a typing-indicator row showing `tool_name: <bytes accumulated>`,
  updating in place — *not* dumping raw partial JSON to the screen
  (would be ~unreadable for big writes anyway).
- Add a `tok/s` readout to the existing `SpinnerGuard`
  (`src/executor/native/agent.rs:1670`). The spinner already has an elapsed
  timer; multiply by the running output-token count and update inline.
- Document the new behavior in `docs/wg-nex.md` (or wherever — point to the
  user-facing nex doc).

**What this does NOT need:**

- Streaming the *tool result* during tool execution. `write_file` is sync
  and instant. `bash` already streams via `execute_streaming` +
  `tool_progress!` (`src/executor/native/tools/progress.rs`). The
  long-running tools (`reader`, `summarize`, `web_fetch`,
  `deep_research`, `chunk_map`, `map`) all already emit progress lines —
  see the doc-comment in `progress.rs:3`.
- Progressive tool execution (the model's "Allow partial tool call execution
  with immediate feedback"). Tools execute as a single atomic operation;
  partial execution is meaningless for `write_file` and dangerous for `bash`.

---

## Report 3 — ECMWF / binary-file / auth / format-parsing

**The model's claims:**

1. *Cannot directly download binary model output files (GRIB format, etc.)*
2. *Cannot access raw model data files that require authentication or specific APIs*
3. *Cannot parse complex visualization files like SVGs, PNGs, or interactive charts*
4. *Cannot download and analyze ensemble model data directly*
5. The 403 on `weather.us/forecast/4641239-memphis/meteogram` is cited as
   evidence of #1.

**What I verified:**

### Claim 1 — "Cannot download binary files (GRIB)"

**Wrong.** `web_fetch` already saves binary content as a file artifact.
`src/executor/native/tools/web_fetch.rs:206 save_binary_artifact` handles any
non-PDF, non-text response by inferring an extension from `Content-Type`
and writing raw bytes to
`<workgraph>/nex-sessions/fetched-pages/NNNNN-<slug>.<ext>` — `.bin` is
the fallback. The `extract_to_markdown` cap (`fetch_max_chars`, default
16000) only applies to the markdown extraction path; binary saves bypass
it entirely.

The model could also have used `bash curl -o out.grib URL` (the `bash` tool
is always available); stdout-capture truncation doesn't apply when curl
writes to a file.

**Caveat:** if the session was launched with `wg nex --minimal-tools`,
`web_fetch` is removed (`src/commands/nex.rs:111` strips everything except
`read_file`, `edit_file`, `write_file`, `bash`, `grep`, `glob`,
`todo_write`). The model would then have to fall back to bash + curl.
That's still possible but the model would not know about it from its tool
list. **This is the documentation gap** to address.

Side note found while looking: `--minimal-tools` keeps `todo_write` but
**no such tool is registered** anywhere — `keep_only_tools` simply filters
to a name that never exists. Harmless but a smell. File a small follow-up.

### Claim 2 — "Cannot access raw model data files that require authentication"

Partial truth. `web_fetch` does not expose `Authorization` / `-u` /
custom headers in its tool schema. The model would need to use `bash curl`.
ECMWF Open Data (https://www.ecmwf.int/en/forecasts/datasets/open-data) is
**unauthenticated** — the model didn't need auth at all here, it just
needed to know the right URL pattern. The 403 it hit was an anti-bot block
(weather.us is a third-party site, not ECMWF), and `web_fetch` already
defends against that with rquest's Chrome-136 TLS fingerprint + headless
Chrome fallback. If the model tried `bash curl https://...` it would get
403. If it tried `web_fetch` it likely would not.

So the real story is: **the model didn't know which tool to use for which
class of URL.** That's a system-prompt / tool-doc fix.

### Claim 3 — "Cannot parse SVGs, PNGs, interactive charts"

This is correctly described but mistakenly framed as a "limitation we
should fix." Parsing rasterized images / PNG charts requires a vision
model — which is out of scope for a text-only Qwen3-Coder. SVG is XML and
the model could read it via `read_file` after a `web_fetch`, but actually
extracting *meaning* (not just markup) from a chart-rendered SVG is a
research problem, not a tool gap. **Won't-fix** on the tool side; the
system prompt should set the expectation honestly.

### Claim 4 — "Cannot analyze ensemble model data directly"

Same shape as #3: GRIB parsing needs a Rust-side `eccodes` binding or a
Python sub-process; ensemble statistics need numpy. The model conflates
"can't download" (false — already addressed) with "can't analyze"
(true — but expected for an LLM with no science library access).
**Won't-fix as filed.** The right pattern is for the model to write a
small Python script, run it via `bash python3`, and read the output. That
flow works today.

**Verdict: model misunderstanding (mostly). One real doc gap: the system
prompt should call out that web_fetch saves binary artifacts and that bash
+ curl is available for auth/API cases.**

**Proposed fix scope (small):**

- Add a paragraph to the default `wg nex` system prompt
  (`src/commands/nex.rs:209`) that names the binary-fetch behavior, the
  bash escape hatch for auth/API calls, and explicitly notes the `bash
  python3 -c` pattern for data-format work that wg's tools don't do
  natively.
- When `--minimal-tools` is set, augment the system prompt with an extra
  note: "no `web_fetch` in this mode — use `bash curl` for HTTP, write to
  file via `-o`."
- Remove the dead `"todo_write"` reference from
  `keep_only_tools` (or actually implement the tool — separate decision).

---

## Follow-up task list (drop-in for `wg add`)

These are written so a downstream task-creator can paste them straight
into shell. None are "draft" — the triage owner picks what to file and
when.

### 1. Stream `InputJsonDelta` so users can see a small model is alive

```bash
wg add 'wg nex: surface tool-input streaming + tok/s readout' \
  -d '## Description
The native executor SSE client (src/executor/native/client.rs:289)
forwards TextDelta to on_text but silently buffers InputJsonDelta. On
slow local models, this means a long write_file looks like a UI freeze
(see docs/agent-reports/tool_call_processing_bug_report.md and
docs/agent-reports/ui_freeze_bug_report.md).

Add a parallel on_tool_input callback. The interactive sink in
src/executor/native/agent.rs should render it as an in-place
typing-indicator row "tool_name: NN bytes" — never dump partial JSON.
Also wire output_tokens / elapsed into SpinnerGuard
(agent.rs:1670) for a live tok/s readout.

## Validation
- [ ] Failing test written first (TDD): on_tool_input fires on
      InputJsonDelta SSE event in messages_streaming_with_callback
- [ ] Manual smoke against wg-nex-style endpoint with a 30B-class
      local model; long write_file shows a non-static row
- [ ] cargo build + cargo test pass with no regressions'
```

### 2. System-prompt update for binary fetch + bash escape hatches

```bash
wg add 'wg nex: system prompt — document binary fetch and bash escape hatches' \
  -d '## Description
Small models do not infer that web_fetch saves binary artifacts (PDF, GRIB,
images) or that bash + curl is the escape hatch for auth/API/format work.
Result: the Qwen3-Coder-30B report
(docs/agent-reports/ecmwf_analysis_limitation.md) declared real
capabilities as missing.

Update the default system prompt in src/commands/nex.rs:209 to include:
1. web_fetch saves binary artifacts (path returned in the tool result)
2. For URLs requiring auth or custom HTTP: use bash + curl
3. For data formats wg does not parse natively (GRIB, NetCDF, etc.):
   write a Python script via bash python3 -c
4. For sites returning 403 to bash curl: web_fetch already does
   Chrome-136 TLS emulation + headless Chrome fallback — try it first

When --minimal-tools is on, add a one-line note: "no web_fetch in this
mode — use bash + curl -o for HTTP."

## Validation
- [ ] Default prompt includes all four points above
- [ ] --minimal-tools prompt addendum present and appears only when flag
      is set
- [ ] cargo build + cargo test pass with no regressions'
```

### 3. Remove dead todo_write reference (or implement it)

```bash
wg add 'wg nex --minimal-tools: drop the dead "todo_write" filter entry' \
  -d '## Description
src/commands/nex.rs:121 includes "todo_write" in keep_only_tools, but no
such tool is registered anywhere in the registry (grep confirms only
this one site references it). The filter call is a no-op for that name.

Either:
a) remove "todo_write" from the keep_only_tools list (5-line change), OR
b) implement a real todo_write tool — separate, larger task.

Pick (a) unless the user has a use case for (b). Found while triaging
docs/agent-reports/*.

## Validation
- [ ] Single source: todo_write removed from
      src/commands/nex.rs:111-122 OR implemented as a real Tool
- [ ] cargo build + cargo test pass'
```

### 4. Won't-fix bookkeeping

No `wg add` for these — recording the decision here in this triage doc is
the bookkeeping:

- **Streaming `write_file`/`edit_file` execution itself**: `fs::write` is
  instant; there is no execution to stream. Won't-fix as filed in
  reports 1+2.
- **Vision parsing for PNGs/charts**: out of scope for the text-only
  native executor. Models that need this should be wired to a
  vision-capable provider via the existing model registry. Won't-fix as
  filed in report 3.
- **GRIB / NetCDF parsing**: write a Python script via `bash`. Won't-add
  as a native tool; the bash escape hatch is the right pattern.

---

## Appendix: relevant source pointers

- `src/executor/native/tools/web_fetch.rs:206 save_binary_artifact` —
  binary download + artifact path (refutes report 3 #1).
- `src/executor/native/tools/web_fetch.rs:51 DEFAULT_MAX_CONTENT_CHARS` —
  the 16000-char cap; markdown only.
- `src/executor/native/client.rs:289 messages_streaming_with_callback` —
  the SSE callback that streams text but not tool input.
- `src/executor/native/client.rs:184 InputJsonDelta` — the
  unstreamed-to-user delta type.
- `src/executor/native/agent.rs:1670` — `SpinnerGuard`, where the tok/s
  readout would go.
- `src/executor/native/agent.rs:1707` — `eprint!`/`flush` for live text.
- `src/commands/nex.rs:111` — `--minimal-tools` filter (the suspect
  behind report 3's "no binary fetch").
- `src/commands/nex.rs:209` — default system prompt the model reads.
- `src/executor/native/tools/progress.rs` — already-working progress
  pipeline for long-running *tools* (not pre-tool model emission).

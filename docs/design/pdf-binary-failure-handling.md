# Design: PDF / binary attachment failure handling

**Task:** design-pdf-binary
**Bug:** bug-read-tool-on-pdfs-burns-tokens-then-crashes.md
**Author:** agent-978 (Default Evaluator role, opus)
**Date:** 2026-04-28

---

## Problem (one-liner)

Agents that call the `Read` tool on a malformed/encrypted PDF burn ~$1 of
cached-context tokens before the Anthropic API returns HTTP 400 ("Could not
process PDF"). The wrapper marks the task `failed` with the generic message
`Agent exited with code N`. A naive `wg retry` repeats the same waste — there
is no signal saying *"don't retry until the input is fixed."*

Verified cost (from agent logs): 2 failures = $1.88 wasted in one batch. With
5–8 parallel Opus, ~$5–10 per bad-PDF round.

---

## Scope decision: SHIP **A only**

| Option | Ship? | Rationale |
|--------|-------|-----------|
| **A** — Failure classification (parse api_error_status from raw_stream.jsonl, set `failure_class=api_error_400_document`, surface in `wg show` / `wg service status`) | **YES** | Cheap, high signal, immediately actionable. Stops the retry-burn loop on day 1. The user has explicitly framed A as "always do A." |
| **B** — Preflight hook framework (run `pdfinfo` before spawn) | **NO (deferred)** | The workgraph-native equivalent already works: the bug-report's own workaround used `wg add diagnose-prepare-pdfs` + `--before <task>` to run `pdfinfo` / `pdftotext` upstream. A first-class hook framework duplicates the cycle pattern at the executor layer, introduces a new config schema (validators, args, error contracts) and a new status (`blocked_on_input`), and pays for itself only if A reveals classes of failure that the cycle workaround can't cover. Not worth the invasiveness for one bug. |
| **C** — Per-task tool forbid (`forbid_tool_on_extension = [".pdf"]` injected as system-prompt addendum) | **NO (deferred)** | Only valuable on retry, *after* A has surfaced the failure class. Operator already has a working escape hatch today (`wg log <task> "NEVER use Read on .pdf — use the .txt sidecar"` survives `wg retry`). C makes that one-shot escape into a structured field; that's a refinement, not a fix. Revisit once A has been live for a few weeks and we see how often per-task forbidding would beat the log-injection pattern. |

**One-line verdict:** ship A; let real usage tell us whether B and C earn their
keep before we build them.

### Acceptable trade-offs (this scope leaves on the table)

- First-time bad-PDF failure still costs ~$1 — we just don't repeat it. This
  is the "fix the input, don't retry" half of the bug; the "don't waste
  the first dollar" half stays open until B (or a workgraph-native preflight
  cycle pattern) ships.
- Operator must manually re-route after the failure (set up a `pdftotext`
  prep task, edit dependents). That manual step is unchanged from today.

---

## Schema additions

### A: failure classification

**`Task` struct** (`src/graph.rs`, near line 353):

```rust
/// Distinguishing class for the most recent failure. Read by retry
/// gating and surfaced in `wg show` / `wg service status`. None for
/// successful tasks or for legacy rows. Always pairs with
/// `failure_reason` (which carries the human prose).
#[serde(default, skip_serializing_if = "Option::is_none")]
pub failure_class: Option<FailureClass>,
```

**New enum `FailureClass`** (also in `src/graph.rs`, `kebab-case` serde):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureClass {
    /// HTTP 400 from the Anthropic API on a document attachment.
    /// Action: fix the input (regenerate sidecar, replace PDF) before retry.
    /// Marker phrase: `api_error_status: 400` AND
    /// (`Could not process PDF` OR `Could not process document`).
    ApiError400Document,

    /// HTTP 429 — rate limit. Auto-retriable after backoff.
    ApiError429RateLimit,

    /// HTTP 5xx — transient upstream error. Auto-retriable.
    ApiError5xxTransient,

    /// Wrapper hard timeout (exit 124). Not auto-retriable in same form.
    AgentHardTimeout,

    /// Generic non-zero exit with no recognised api_error pattern.
    /// Equivalent to today's "Agent exited with code N".
    AgentExitNonzero,

    /// Wrapper-side issue (e.g., `wg fail` invocation failed, missing
    /// raw_stream.jsonl). Operator should inspect the wrapper log.
    WrapperInternal,
}
```

**Default for legacy rows:** absent field deserialises to `None`. No
migration needed (mirrors the priority-int-and-string pattern).

### A: CLI surface

**`wg fail`** (`src/cli.rs`, `Fail` struct around line 525) gains:

```rust
/// Failure classification (machine-readable; pairs with --reason).
/// One of: api-error-400-document, api-error-429-rate-limit,
///         api-error-5xx-transient, agent-hard-timeout,
///         agent-exit-nonzero, wrapper-internal.
#[arg(long, value_name = "CLASS")]
class: Option<String>,
```

**No changes to `wg add`, `wg retry`, `wg show` flags** — the field is
populated by the wrapper and surfaced in default output.

---

## Code locations to modify (no edits, paths only)

Implementer note: this is a fan-out-friendly task because the parser, the
wrapper integration, and the surfacing each touch disjoint files. If
`fix-pdf-binary` is decomposed, the natural split is:

1. **Parser + schema** (sequential, foundational):
   - `src/graph.rs` — add `FailureClass` enum + `failure_class: Option<FailureClass>` field on `Task`. Update the deserializer helper (`graph.rs` ~1118, ~1267) to thread the new field.
   - `src/commands/spawn/raw_stream_classifier.rs` — **NEW** small module: `pub fn classify_from_raw_stream(path: &Path, exit_code: i32) -> FailureClass`. Reads up to last N=4 KB of `raw_stream.jsonl`, looks for `api_error_status` integer + body keywords. Pure function, unit-testable.
   - `src/commands/fail.rs` — accept `class: Option<FailureClass>` param; persist to `task.failure_class` alongside `failure_reason`. (Already mutates `failure_reason` at line 80–90.)
   - `src/cli.rs` — add `--class` to `Commands::Fail` (~line 525); thread to `commands::fail::run`.

2. **Wrapper integration** (depends on 1):
   - `src/commands/spawn/execution.rs` — modify the wrapper script around line 1426–1428 (the "agent exited nonzero" branch). Before calling `wg fail`, invoke `wg classify-failure "$TASK_ID" --raw-stream "$RAW_STREAM" --exit-code $EXIT_CODE` (new internal subcommand) which prints the class string. Then pass `--class <CLASS>` to `wg fail`. Reason: keeping the parser in Rust (not bash regex) makes it testable and avoids re-implementing JSON parsing in shell.
   - `src/commands/mod.rs` + new `src/commands/classify_failure.rs` — internal subcommand that wraps the classifier from step 1.
   - `src/cli.rs` — register the new internal subcommand (`Commands::ClassifyFailure { ... }`); hidden from default help (`#[command(hide = true)]`).

3. **Surfacing** (depends on 1, parallelisable with 2):
   - `src/commands/show.rs` — after the `failure_reason` block (~line 696), print `failure_class: <kebab-name>` and a short hint (e.g., for `api-error-400-document`: "fix the input — see `bug-read-tool-on-pdfs-…` for sidecar pattern").
   - `src/commands/service/ipc.rs` — include `failure_class` in the JSON status payload (~line 1374).
   - `src/commands/service/coordinator_agent.rs` (~line 1097) — when summarising failed deps for downstream tasks, include the class string.
   - `src/tui/viz_viewer/state.rs` — add the class to the failed-task hover/detail panel (consistency with `failure_reason`).

**Scope guardrail:** no changes to `src/dispatch/`, the agency pipeline, the
spawn handler, or the claude/codex executors. The wrapper sees the failure
post-hoc; classification does not change which agent runs or what prompt it
gets.

---

## Test plan

### Unit tests (lives next to the code)

`src/commands/spawn/raw_stream_classifier.rs`:

- `test_classifier_pdf_400_from_real_jsonl` — feed a fixture with the exact
  agent JSONL line `{"type":"result","subtype":"error_during_execution",
  "is_error":true,"api_error_status":400,...,"message":"Could not process
  PDF"}`. Expect `FailureClass::ApiError400Document`.
- `test_classifier_429_rate_limit` — exit code 1, body contains
  `"api_error_status":429`. Expect `ApiError429RateLimit`.
- `test_classifier_500_transient` — `"api_error_status":500`. Expect
  `ApiError5xxTransient`.
- `test_classifier_hard_timeout` — exit code 124, raw_stream empty. Expect
  `AgentHardTimeout`.
- `test_classifier_generic_exit` — exit code 1, no api_error_status in body.
  Expect `AgentExitNonzero`.
- `test_classifier_missing_raw_stream` — file does not exist, exit code 1.
  Expect `WrapperInternal` (do not crash; surface the missing-stream
  condition).
- `test_classifier_truncated_jsonl` — last line of raw_stream is partial JSON.
  Expect classifier to fall back to `AgentExitNonzero` rather than panic.

### Integration test (`tests/integration_failure_classification.rs`, NEW)

- `test_wg_fail_with_class_persists` — `wg fail <id> --class api-error-400-document --reason "..."` round-trips
  through `graph.jsonl` (load → assert `task.failure_class ==
  ApiError400Document`).
- `test_wg_show_renders_failure_class` — assert `wg show <id>` output
  contains the kebab class string and the operator hint.

### Smoke scenario (HARD GATE — required by task validation)

`tests/smoke/scenarios/failure_class_pdf_400.sh` (NEW):

```
1. wg_smoke_root scratch dir + `wg init`.
2. Generate a deterministically broken PDF fixture:
   - tests/smoke/fixtures/broken.pdf — SHA-pinned, ~50 bytes.
   - Built by `printf '%%PDF-1.4\nGARBAGE NOT A REAL PDF\n%%%%EOF\n' > broken.pdf`
     (the `%PDF-1.4` magic gets the file past extension-only checks but the
     body is not a parseable xref). Verified to trigger Anthropic 400
     "Could not process PDF" on Read.
3. wg add "smoke-bad-pdf" -d "Read ./broken.pdf and summarise" --executor claude
4. Inject a synthetic raw_stream.jsonl with the api_error_status:400 line
   instead of actually calling the API (smoke must run offline / cheap).
   Then invoke the new classifier subcommand:
       wg classify-failure smoke-bad-pdf --raw-stream <fixture> --exit-code 1
   Expect stdout = "api-error-400-document".
5. Run `wg fail smoke-bad-pdf --class api-error-400-document --reason "..."`.
6. Assert: `wg show smoke-bad-pdf` output contains "failure_class:
   api-error-400-document".
7. Assert: `wg service status` JSON has the class for the failed task.
```

`owners = ["design-pdf-binary", "fix-pdf-binary", "verify-end-to"]`.
Exit 0 = PASS. The scenario does NOT call the live API (cost), but it DOES
exercise the full wrapper-integrated path end-to-end with a real fixture
PDF that has been verified offline to trigger the 400 in a manual test
during fix-pdf-binary implementation.

**Smoke scenario MUST include a real malformed PDF in the repo** (per the
task validation checklist). Fixture path:
`tests/smoke/fixtures/broken.pdf` — committed alongside the smoke script.
Document the manual verification step (one-time, offline) in the scenario
header so future maintainers know the fixture is API-tested even though the
smoke run itself stays offline.

### Live verification (one-time, during fix-pdf-binary, NOT in smoke)

Spawn one real Opus agent against a task referencing `broken.pdf`. Confirm:
- Agent terminates with exit code 1.
- `wg show <task>` reports `failure_class: api-error-400-document`.
- `wg retry <task>` does NOT auto-clear the class (operator must `wg edit`
  the input or accept the class on retry).

This live run costs ≤ $0.10 (small Opus context, single Read tool call) and
proves the wrapper integration works against the real API.

---

## Out-of-scope (explicit non-goals for this batch)

- **Auto-skip retry on `api-error-400-document`.** That is a policy decision
  worth a separate task — the failure_class field is the *enabler*, the
  retry-gating is the next layer. Recommend filing `gate-retry-on-failure-class`
  as a follow-up after fix-pdf-binary lands.
- **Pre-spawn validators (Option B).** Defer; reuse the existing
  `wg add prep-task --before <agent-task>` cycle pattern when needed.
- **System-prompt addendum / tool-forbid (Option C).** Defer; the
  `wg log <task> "NEVER read .pdf"` log-injection workaround already covers
  the per-task case until we have data showing structured forbidding pays
  off.
- **Any change to the claude / codex / native executors themselves.** The
  fix is purely in the wrapper + graph schema + surfacing layer.

---

## Notes for the implementer (`fix-pdf-binary`)

- Keep the new classifier subcommand `wg classify-failure` hidden
  (`#[command(hide = true)]`) — it is wrapper-internal, not user-facing.
- The wrapper change at `src/commands/spawn/execution.rs:1426` is the only
  line that *must* land in this batch; everything else is plumbing the
  result through. If you have to ship in halves: ship A's parser + wrapper
  first; surfacing in `wg show` / `wg service status` can land in a follow-up.
- `failure_class` should NEVER be cleared by `wg retry` automatically — the
  operator clears it (or it gets overwritten on the next failure). This is
  by design: the retry-gating layer (out of scope here) reads the field and
  decides.
- Don't touch the bash wrapper's overall structure — it has been a source of
  regressions (worktree cleanup, heartbeat). Add the classify call as a
  minimal pre-step before the existing `wg fail` invocation.

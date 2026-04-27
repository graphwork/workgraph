# Smoke gate (`wg done` regression contract)

This directory holds the **smoke manifest** — a structured TOML file plus a
set of shell scripts. The manifest is the regression contract for `wg done`:
**a task cannot be marked done while a smoke scenario it owns is failing.**

## How it works

`wg done <task-id>` does the following before mutating state:

1. Loads `tests/smoke/manifest.toml` (path overridable with
   `WG_SMOKE_MANIFEST=...`).
2. Selects scenarios whose `owners = [...]` list contains the task id, OR
   every scenario when `--full-smoke` is passed.
3. Runs each selected scenario with `bash <script>`.
4. Inspects the exit code:
   * **0** → PASS
   * **77** → loud SKIP (precondition missing — endpoint unreachable, no
     credentials, etc.); does not block `wg done`
   * **anything else** → FAIL; `wg done` exits non-zero with the broken
     scenario name(s)

Agents (`WG_AGENT_ID` set in env) cannot bypass the gate via `--skip-smoke`
unless `WG_SMOKE_AGENT_OVERRIDE=1` is also set in the same shell. This is
deliberate — agents claiming done is the failure mode the gate exists to
prevent.

Humans can pass `--skip-smoke` (a loud warning is printed) when they
understand why a particular scenario is not load-bearing for their change.

## Adding a scenario (this is grow-only)

Every regression that should have been caught by smoke gets a permanent
scenario here. Do not delete entries; extend.

1. Drop a script under `tests/smoke/scenarios/<name>.sh` that exits
   0/77/non-zero per the contract above.
2. Add a `[[scenario]]` block to `manifest.toml` with `name`, `script`
   (relative to the manifest), `owners` (the task id(s) this scenario
   protects), and `description`.
3. List `smoke-gate-is` in `owners` so the manifest's own ground-truth
   scenarios always run when modifying the gate.
4. Source `_helpers.sh` for the `loud_skip` / `loud_fail` / `require_wg` /
   `endpoint_reachable` helpers — the SKIP/FAIL banners are greppable.

## Live, not stubs

Scenarios MUST hit real endpoints / real binaries. The original wave-1 smoke
silently passed against a fake LLM and that's exactly how the wg-nex 404
shipped to users. If you need a stubbed scenario, write a unit test instead
— do not put it in this manifest.

## No eyeball gates

Every scenario MUST produce a programmatically-assertable text or data
stream — never "human looks at the terminal and judges." Each script states
the expected output (literal text, JSON shape, file content, log line) and
asserts on it. "Did not crash" is not enough. "Returned a non-error" is not
enough on its own — also assert the positive marker (role=coordinator, the
expected file appears, the expected substring is in the log, etc.).

If you cannot articulate the expected output as a grep/jq/diff, the scenario
is not ready to ship. Recent example: a Log view bug shipped because the fix
worked at the file layer but the rendering pipeline silently dropped lines —
no scenario asserted "after opening the Log view, output contains lines
{1..N} of the expected text." That's exactly the gap this manifest exists to
close.

## Initial scenarios

| Scenario | Protects | What it does |
|---|---|---|
| `nex_two_message_against_lambda01` | wg-nex-native* | Live two-message chat against lambda01 (qwen3-coder) |
| `dispatcher_boot_no_orphan_supervisor` | rename-dispatcher-daemon, bug-a-regression-test | Boots dispatcher, asserts no orphan / ghost coordinator entry |
| `claude_executor_with_global_openrouter_default` | model-is-not, wire-priority-field | Local claude + global openrouter is_default → no native-exec leak |
| `priority_int_and_string_deserialize` | wire-priority-field | graph.jsonl with int/string/map priority forms reads cleanly |
| `chat_create_via_ipc_works` | wg-nex-native, fix-tui-coordinator-2, fix-tui-new | Single `wg chat 'hi'` succeeds against a fresh claude-executor project |

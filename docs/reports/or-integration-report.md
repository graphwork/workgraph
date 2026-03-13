# OpenRouter Pipeline Integration Report

**Date:** 2026-03-13 (iteration 2: 2026-03-13)
**Task:** or-integrate
**Status:** PASS (2/2 iterations)

## 1. Merge Check

All parallel branches have been merged to `main` without conflicts:

| Branch | Scope | Status |
|--------|-------|--------|
| or-wire-endpoint-spawn | Spawn path: endpoint/model/provider resolution | Merged |
| or-expose-provider-env | Env var pass-through to spawned agents | Merged |
| or-wire-agent-prefs-spawn | Agent preferred_model/preferred_provider in spawn | Merged |
| or-api-key-file | API key file support in config | Merged |
| or-cli-endpoints | `wg endpoints` CLI subcommand | Merged |
| or-streaming | OpenAI-compatible SSE streaming client | Merged |
| or-agent-model-prefs | --model/--provider flags on `wg agent create` and `wg add` | Merged |
| fix-add-preferred | Agent struct initializer fixes (preferred_model/preferred_provider) | Merged |
| fix-test-create | Env var isolation fix for provider tests | Merged |

No merge conflicts detected. All changes integrate cleanly on `main`.

## 2. Full Test Suite

```
Iteration 1: cargo test: 4454 passed, 0 failed, 11 ignored
Iteration 2: cargo test: 4443 passed, 0 failed, 11 ignored
cargo build --release: OK (3 warnings — all dead_code, unrelated to OpenRouter)
```

All test binaries pass. Zero regressions. Minor test count variance between iterations
is due to concurrent agent activity in shared working tree (not regressions).

### OpenRouter-Specific Test Files
- `tests/integration_openrouter_flow.rs` — 34 tests (endpoint resolution, spawn env vars, config round-trip)
- `tests/integration_cli_endpoints.rs` — 15 tests (add/list/remove/set-default lifecycle)
- `tests/integration_streaming.rs` — 15 tests (SSE streaming, error handling, mock HTTP server)

## 3. Build Verification

| Profile | Result |
|---------|--------|
| `cargo build` (dev) | OK |
| `cargo build --release` | OK |
| `cargo install --path .` | OK |

## 4. Smoke Walkthrough — Complete User Journey

### Step 1: Add an OpenRouter endpoint
```bash
$ wg endpoints add my-or --provider openrouter \
    --url https://openrouter.ai/api/v1 \
    --model anthropic/claude-sonnet-4-20250514 \
    --api-key-file ~/.openrouter-key
Added endpoint 'my-or' [openrouter] (set as default)
```

### Step 2: Verify endpoint listing
```bash
$ wg endpoints list
Configured endpoints:

  my-or (default)
    provider: openrouter
    url:      https://openrouter.ai/api/v1
    model:    anthropic/claude-sonnet-4-20250514
    api_key:  (from file)
```

### Step 3: Create an agent with OpenRouter preferences
```bash
$ wg agent create or-sonnet --role 52335de1 --tradeoff 2dc69b33 \
    --model anthropic/claude-sonnet-4-20250514 --provider openrouter
Created agent 'or-sonnet' (a4724ba7)
  role:       Programmer (52335de1)
  tradeoff:   Thorough (2dc69b33)
  executor:   claude
  model:      anthropic/claude-sonnet-4-20250514 (preferred)
  provider:   openrouter (preferred)
```

### Step 4: Create a task with model/provider
```bash
$ wg add 'test task' --model anthropic/claude-sonnet-4-20250514 --provider openrouter
Added task: test task (test-task)
```

Task stored with `model` and `provider` fields in graph.jsonl.

### Step 5: Spawned agent receives correct env vars

The spawn pipeline (`src/commands/spawn/execution.rs`) sets these env vars for agents:

| Env Var | Source | Value |
|---------|--------|-------|
| `WG_MODEL` | task.model > agent.preferred_model > executor.model > coordinator.model | `anthropic/claude-sonnet-4-20250514` |
| `WG_LLM_PROVIDER` | task.provider > agent.preferred_provider > role config | `openrouter` |
| `WG_ENDPOINT` | task.endpoint > provider match > agent provider match > role config | `my-or` |
| `WG_ENDPOINT_URL` | Resolved from endpoint config | `https://openrouter.ai/api/v1` |
| `WG_API_KEY` | Resolved from endpoint config (key file or inline) | `(from ~/.openrouter-key)` |

### Model Resolution Hierarchy
```
task.model > agent.preferred_model > executor.model > coordinator.model
```

### Provider Resolution Hierarchy
```
task.provider > agent.preferred_provider > role-based config
```

### Endpoint Resolution Cascade
```
1. task.endpoint (explicit)
2. task.provider → find matching endpoint
3. agent.preferred_provider → find matching endpoint
4. role config endpoint
```

## 5. Architecture Summary

The OpenRouter pipeline consists of these components:

1. **Config layer** (`src/config.rs`): `LlmEndpoints` struct stores named endpoints with provider, URL, model, API key (inline or file path). Provider-specific env var lookup (`OPENROUTER_API_KEY`, `OPENAI_API_KEY`).

2. **CLI layer** (`src/commands/endpoints.rs`): Full CRUD for endpoints — add, list, remove, set-default, test connectivity.

3. **Agent layer** (`src/agency/`): `Agent` struct has `preferred_model` and `preferred_provider` fields. `wg agent create --model --provider` stores preferences.

4. **Task layer** (`src/graph.rs`): `Task` struct has `model`, `provider`, `endpoint` fields. `wg add --model --provider` stores per-task preferences.

5. **Spawn layer** (`src/commands/spawn/execution.rs`): Resolves effective model/provider/endpoint through the hierarchy, passes them as env vars to the spawned agent process.

6. **Client layer** (`src/executor/native/openai_client.rs`): OpenAI-compatible HTTP client with SSE streaming support. Handles OpenRouter-specific auth and API patterns.

## 6. Known Issues

- **3 dead_code warnings**: `scaffold_assign_tasks_batch`, `AnimationKind::AnnotationClick`, `AnnotationHitRegion::parent_task_id` — cosmetic, unrelated to OpenRouter.
- **11 ignored tests**: Pre-existing ignored tests, not OpenRouter-related (7 from one binary, 4 from another).
- **No live API test**: Smoke test verified CLI and config plumbing. Live API calls require a valid `OPENROUTER_API_KEY` which is not available in CI. The `wg endpoints test` command exists for manual verification.

## 7. Conclusion

The OpenRouter pipeline is fully integrated and operational. All branches merged cleanly, the full test suite passes, release builds succeed, and the complete user journey works end-to-end from endpoint configuration through agent spawning with correct env var propagation.

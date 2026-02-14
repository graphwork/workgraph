# Review: Configuration, Build & CI

## 1. Feature Flag System

### Current Feature Flags (Cargo.toml)

| Flag | Default | Dependencies Gated | Purpose |
|------|---------|-------------------|---------|
| `matrix-lite` | Yes (in `default`) | `reqwest`, `urlencoding` | Lightweight Matrix integration via plain HTTP. No E2EE, no SQLite. |
| `matrix` | No | `matrix-sdk`, `futures-util` | Full Matrix integration with E2EE and SQLite-backed crypto store. |
| `llm-tests` | No | (none) | Test-only flag gating integration tests that call the Claude CLI. |

### How Feature Flags Are Used

- **`matrix`**: Guards `src/matrix/` module, Matrix-related imports in `src/lib.rs`, and notification paths in `src/commands/notify.rs`, `src/commands/claim.rs`, `src/commands/matrix.rs`.
- **`matrix-lite`**: Guards `src/matrix_lite/` module, its re-exports in `src/lib.rs`, and a notification call in `src/agency.rs:220`.
- **`llm-tests`**: Guards two test functions — one in `tests/integration_auto_assignment.rs:894` and one in `tests/integration_service_coordinator.rs:980`. Run with `cargo test --features llm-tests`.

### Assessment

The feature flag structure is **clean and well-motivated**:
- Matrix has two tiers (full SDK vs. HTTP-only) which makes sense for avoiding heavy native deps (SQLite, OpenSSL for E2EE) when only notifications are needed.
- `llm-tests` correctly gates tests that require external services (Claude CLI) so CI doesn't need API keys.
- The `default = ["matrix-lite"]` choice is sensible — users get notifications out of the box without the heavy `matrix-sdk` dependency.

## 2. Dependency Analysis

### Core Dependencies (always included)

| Crate | Version | Used In | Purpose |
|-------|---------|---------|---------|
| `serde` + `serde_json` | 1.0 | Everywhere | Serialization (YAML configs, JSON stats, graph format) |
| `clap` (derive) | 4.4 | `src/main.rs` | CLI argument parsing |
| `petgraph` | 0.6 | **Not imported in any `.rs` file** | Graph algorithms |
| `thiserror` | 2.0 | `src/parser.rs`, `src/agency.rs` | Error type derivation |
| `anyhow` | 1.0 | Everywhere | Error propagation |
| `chrono` (serde) | 0.4 | Task timestamps, usage logging | Date/time handling |
| `toml` | 0.8 | `src/config.rs` | Config file parsing |
| `serde_yaml` | 0.9 | Agency YAML files | YAML serialization for agency system |
| `libc` | 0.2 | `src/commands/service.rs`, `src/parser.rs`, etc. | Unix process management (kill, signal, PID) |
| `dirs` | 5.0 | `src/config.rs`, `src/agency.rs` | XDG config directory resolution |
| `sha2` | 0.10 | `src/agency.rs` | Content-hash agent identities |
| `tokio` | 1 (rt-multi-thread, macros, sync, time) | `src/commands/notify.rs`, `src/commands/matrix.rs`, `src/matrix/` | Async runtime for Matrix integration |
| `ratatui` | 0.29 | `src/tui/mod.rs` | Terminal UI framework |
| `crossterm` | 0.28 | `src/tui/mod.rs` | Terminal backend for ratatui |
| `ascii-dag` | 0.8 | `src/tui/dag_layout.rs` | DAG rendering for TUI |

### Issues Found

#### P0: Failing Unit Tests (Stale Assertions)

Two tests in `src/config.rs` fail because the default model was changed from `"opus-4-5"` to `"opus"` (commit `2a6763e`) but the test assertions were not updated:

```
config::tests::test_default_config    — asserts model == "opus-4-5", actual is "opus"
config::tests::test_build_command     — asserts cmd.contains("opus-4-5"), actual contains "opus"
```

These tests fail on `cargo test --lib`, which means **CI is currently red** on the `build` job (or the change hasn't been pushed yet). The fix is trivial — update the assertions to `"opus"`.

#### P1: Duplicate `libc` Dependency

`libc = "0.2"` appears twice in Cargo.toml:
1. Line 28: unconditional `[dependencies]`
2. Line 44-45: `[target.'cfg(unix)'.dependencies]`

The unconditional entry supersedes the target-specific one. Since workgraph only targets Unix anyway (the daemon script, PID management, and `kill()` calls all assume Unix), the target-specific block on line 44-45 is redundant and should be removed.

#### P2: `petgraph` Appears Unused

`petgraph` is declared as a dependency but no source file contains `use petgraph`. It's possible it's used via `petgraph::` paths without a top-level `use` import — but a project-wide grep for `petgraph::` also returns no hits (only docs). This dependency (with its transitive deps) should be verified and removed if truly unused.

#### P3: `tokio` Is Only Used by Matrix Features

`tokio` (with `rt-multi-thread`, `macros`, `sync`, `time`) is always compiled but only used in:
- `src/commands/notify.rs`
- `src/commands/matrix.rs`
- `src/matrix/mod.rs`

All of these are gated behind `#[cfg(feature = "matrix")]` or `matrix-lite`. `tokio` should ideally be made optional and gated behind the matrix features to reduce compile time for users who don't need notifications.

#### P4: `serde_yaml` Deprecation

The `serde_yaml` crate (0.9) is [officially deprecated/unmaintained](https://github.com/dtolnay/serde-yaml). The recommended replacement is `serde_yml`. This is low-priority since 0.9 works fine, but worth planning a migration.

#### P5: Doc Comments Reference Stale Model Name

Several doc comments in `config.rs` still say `"opus-4-5"` (lines 124, 168) as an example model name, which is stale after the default was changed to `"opus"`.

### Dev-Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `tempfile` | 3.10 | Temp dirs for config/usage tests |
| `serial_test` | 3 | Serializing integration tests that share state |

Both are appropriate and actively used.

## 3. CI Pipeline Analysis

### Current Jobs

| Job | Runs On | What It Does |
|-----|---------|--------------|
| `check` | ubuntu-latest, stable | `cargo fmt --check` + `cargo clippy -- -D warnings` |
| `build` | ubuntu-latest, stable | `cargo build` + `cargo test --lib` + `cargo test --doc` |
| `integration` | ubuntu-latest, stable, 15min timeout | `cargo install --path .` + `cargo test --test integration_service -- --test-threads=1` |
| `nightly` | ubuntu-latest, nightly, `continue-on-error: true` | `cargo build` + `cargo test --lib` |

### Strengths

- **Good separation**: lint, build/unit-test, integration, and nightly are separate jobs for clear failure signals.
- **Caching**: All jobs cache `~/.cargo/registry`, `~/.cargo/git`, and `target/` keyed on `Cargo.lock` hash.
- **Nightly canary**: `continue-on-error: true` correctly avoids blocking PRs while surfacing regressions.
- **Integration timeout**: 15 minute timeout with per-step 10 min limit prevents runaway tests.
- **`cargo install` before integration tests**: Correctly mirrors the user workflow where `wg` CLI must be on PATH.

### Issues & Gaps

#### P0: CI Is Likely Failing (Stale Test Assertions)

The `build` job runs `cargo test --lib`, which includes the two failing `config::tests` mentioned above. This means the build job is red.

#### P1: Only 1 of 9 Integration Test Files Is Run in CI

The `integration` job only runs `integration_service`. The other 8 test files are never run in CI:
- `integration_agency.rs`
- `integration_agency_edge_cases.rs`
- `integration_agency_lineage.rs`
- `integration_auto_assignment.rs`
- `integration_review_workflow.rs`
- `integration_service_coordinator.rs`
- `evaluation_recording.rs`
- `skill_resolution.rs`

This is a significant coverage gap. These tests should be added to CI (possibly as a separate job with `--test-threads=1` since they likely share state).

#### P2: No Matrix Feature CI Builds

Neither `matrix` nor `matrix-lite` features are explicitly tested in CI. The default build includes `matrix-lite`, but the full `matrix` feature (which pulls in `matrix-sdk` with SQLite/E2EE) is never compiled in CI. Feature-gated code could silently break.

#### P3: No `cargo test --tests` (Binary Integration Tests)

The `build` job runs `--lib` and `--doc` but not `--tests` (which would include `tests/*.rs`). This means even simple non-service integration tests don't run in the `build` job.

#### P4: Duplicate Toolchain + Cache Setup

Each of the 4 jobs independently installs the Rust toolchain and sets up caching. This could be DRYed with a reusable workflow or composite action, but this is cosmetic — GitHub Actions caches are shared across jobs in the same workflow run, so the actual cache hit rate is fine.

#### P5: No Release/Cross-Compilation Builds

There's no CI for building release binaries or cross-compiling to macOS/aarch64. This is fine for the current stage but worth adding when distributing binaries.

## 4. Build System (Cargo.toml)

### Edition & Toolchain

- **Edition 2024**: The project uses Rust edition 2024, which requires Rust 1.85+. The current system has rustc 1.93.0. CI uses `dtolnay/rust-toolchain@stable` which will get the latest stable, so this is fine.
- **`#![recursion_limit = "256"]`** in `lib.rs`: Likely needed by derive macros on deeply nested types. Worth a comment explaining why, or investigating if still needed.

### Binary

Single binary `wg` from `src/main.rs`. Clean and simple.

### Workspace

Not a workspace — single crate. Appropriate for the current codebase size (~53k lines across all `.rs` files).

## 5. Supporting Scripts

### `scripts/wg-daemon.sh`

A shell-based daemon wrapper for the `wg agent` command. Features:
- Start/stop/restart/status/logs subcommands
- PID file management in `.workgraph/pids/`
- Log rotation to `.workgraph/logs/`
- Automatic restart on non-zero exit with 5s backoff
- Graceful shutdown (SIGTERM, 30s timeout, then SIGKILL)

**Assessment**: This script appears to be a **legacy predecessor** to the Rust-native `wg service start` command, which now handles daemon management internally. If that's the case, it should be documented as such or removed. If it's still used for specific workflows, it should be mentioned in docs.

**Minor issue**: The `CMD` construction on line 67 (`CMD="$WG_BIN agent $ACTOR $*"`) uses `$*` which doesn't preserve argument quoting. Should use `"$@"` instead.

### `.gitignore`

```
/target
.workgraph/
USER_FEEDBACK.md
agency_session_id.txt
```

Clean and appropriate. The `.workgraph/` exclusion prevents accidentally committing project state. `USER_FEEDBACK.md` and `agency_session_id.txt` are ephemeral session files.

## 6. Configuration System (`src/config.rs` + `src/commands/config_cmd.rs`)

### Architecture

Two config files with separate concerns:
- **`.workgraph/config.toml`** (per-project): Agent, coordinator, project, help, and agency settings.
- **`~/.config/workgraph/matrix.toml`** (per-user, global): Matrix credentials. Correctly separated to avoid committing secrets.

### Config Sections

| Section | Fields | Purpose |
|---------|--------|---------|
| `[agent]` | executor, model, interval, command_template, max_tasks, heartbeat_timeout | Per-agent defaults |
| `[coordinator]` | max_agents, interval, poll_interval, executor, model | Service daemon settings |
| `[project]` | name, description, default_skills | Project metadata |
| `[help]` | ordering | Help command display ("usage", "alphabetical", "curated") |
| `[agency]` | auto_evaluate, auto_assign, assigner/evaluator/evolver agents & models, retention_heuristics | Evolutionary identity system |

### Assessment

- Config is well-structured with clear separation of concerns.
- All fields have sensible defaults via `#[serde(default)]`.
- The `command_template` with `{model}`, `{prompt}`, `{task_id}`, `{workdir}` placeholders is flexible.
- The `config_cmd.rs` update function takes 17 parameters — this is verbose but functional. Could benefit from a builder pattern or config-key-value setter approach (e.g., `wg config set agent.model haiku`).

### Help/Usage System (`src/usage.rs`)

The usage tracking system is well-designed:
- **Write path**: O(1) append-only log using `O_APPEND` (atomic on POSIX, no locking needed).
- **Aggregation**: Service daemon periodically merges log into `stats.json` and truncates the log.
- **Read path**: Pre-aggregated stats for fast help ordering.
- **Cold start**: `DEFAULT_ORDER` provides sensible ordering before enough data accumulates (minimum 20 invocations).
- **Tiering**: Commands are grouped into Frequent (>=10%), Occasional (>=2%), Rare (<2%).

No issues found — the design is solid and well-tested.

## 7. Summary of Recommendations

### Immediate (P0)

1. **Fix failing tests**: Update `test_default_config` and `test_build_command` in `src/config.rs` to assert `"opus"` instead of `"opus-4-5"`.

### High Priority (P1)

2. **Remove duplicate `libc`**: Delete the `[target.'cfg(unix)'.dependencies]` block (lines 44-45) since `libc` is already unconditionally included.
3. **Add remaining integration tests to CI**: Add a job (or expand the existing one) to run all 9 test files in `tests/`, not just `integration_service`.

### Medium Priority (P2)

4. **Verify and remove `petgraph`**: If truly unused, remove from Cargo.toml to reduce compile time and dependency surface.
5. **Add `--features matrix` CI build**: Ensure the full Matrix feature compiles on every PR.
6. **Gate `tokio` behind matrix features**: Make it optional to speed up non-Matrix builds.

### Low Priority (P3+)

7. **Plan `serde_yaml` -> `serde_yml` migration**: The crate is deprecated; migrate when convenient.
8. **Update stale doc comments**: Change `"opus-4-5"` examples to `"opus"` in config.rs doc comments.
9. **Clarify `wg-daemon.sh` status**: Document whether it's still needed alongside `wg service start`, or remove it.
10. **Fix `$*` -> `"$@"`** in `wg-daemon.sh` line 67 for correct argument handling.
11. **Consider `wg config set key value`**: Simplify the 17-parameter `update()` function into a generic key-value setter.

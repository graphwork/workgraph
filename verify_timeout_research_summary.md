# Verify Timeout Research Summary

## Overview
Investigation of the workgraph verify timeout mechanism, configuration, and contention patterns.

## Key Findings

### 1. Timeout Implementation & Configuration

**Default Timeout**: 300 seconds (5 minutes)
- **Source**: `src/commands/done.rs:74` - Uses `WG_VERIFY_TIMEOUT` environment variable with fallback to 300
- **Configuration**: Can be overridden via `WG_VERIFY_TIMEOUT` environment variable
- **Implementation**: Uses polling loop with `try_wait()` and `std::thread::sleep(Duration::from_millis(100))`

**Code Reference**:
```rust
let timeout_secs: u64 = std::env::var("WG_VERIFY_TIMEOUT")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(300);
```

### 2. Timeout Behavior

**When Timeout Fires**:
- Process is killed via `child.kill()`
- Returns `VerifyOutput` with `exit_code: "timeout"`
- Error message: `"Verify command timed out after {}s"`
- **No immediate retry** - timeout counts as a verification failure

**Agent Timeout vs Verify Timeout**:
- **Agent timeout**: Default 30 minutes (`config.rs:2366`) - wraps entire agent execution with `timeout` command
- **Verify timeout**: Default 5 minutes - specifically for `wg done --verify` command execution

### 3. Consecutive Failure Counter

**Configuration**: `src/config.rs:2261-2266`
```rust
/// Maximum consecutive verify command failures before a task is auto-failed.
/// When a task's verify command fails this many times in a row, the task
/// transitions to Failed with a descriptive error. Default: 3.
#[serde(default = "default_max_verify_failures")]
pub max_verify_failures: u32,

fn default_max_verify_failures() -> u32 {
    3
}
```

**Behavior**:
- After 3 consecutive verify failures, task auto-transitions to Failed status
- Counter resets on successful verification
- Visible in task output as `[V!N]` where N is the failure count

### 4. Verify vs Agent Timeouts

**Agent Timeout (spawn execution)**:
- Location: `src/commands/spawn/execution.rs:476-480`
- Uses system `timeout` command with `--signal=TERM --kill-after=30`
- Exit code 124 indicates agent was killed by hard timeout
- Wrapper script detects this and marks task as failed with reason "Agent exceeded hard timeout"

**Verify Timeout (task completion)**:
- Location: `src/commands/done.rs:71-100`
- Internal polling mechanism
- No external `timeout` command used
- Returns structured error with timeout indication

### 5. Worktree Isolation & Cargo Contention

**Worktree System**:
- **Enabled**: Via `config.coordinator.worktree_isolation` (default: false)
- **Implementation**: Each agent gets isolated git worktree at `.wg-worktrees/<agent-id>/`
- **Branch**: `wg/<agent-id>/<task-id>`
- **Symlink**: `.workgraph` directory symlinked into each worktree

**Cargo Target Directory**:
- **No isolation found** - agents share the same `target/` directory in project root
- **No CARGO_TARGET_DIR** configuration detected
- **Lock contention**: Multiple agents running `cargo test` simultaneously will contend on:
  - `target/.rustc_info.json`
  - `target/debug/incremental/` directories
  - `Cargo.lock` file (though read-mostly)

**Worktree Setup Hook**:
- Optional `worktree-setup.sh` script can be placed in `.workgraph/` directory
- Runs during worktree creation but no custom script found in this project
- Could be used to configure per-agent `CARGO_TARGET_DIR`

### 6. Recent Failure Analysis

**Pattern Observed**:
- Many agent logs contain timeout patterns (found ~1000+ logs with timeout references)
- Failed tasks include verify failures marked with `[V!3]` indicating consecutive failures
- Examples of verify timeout vs genuine test failures:
  - Timeout: Process killed after 300s with `exit_code: "timeout"`
  - Genuine failure: Test failure with specific error messages and normal exit codes

### 7. Fork-Bomb Dynamics & Throttling

**Concurrent Agent Limit**:
- **Configuration**: `src/config.rs:2297-2299`
- **Default**: 8 concurrent agents maximum (`max_agents`)
- **Purpose**: Prevents unlimited agent spawning

**Cascade Mechanism**:
1. **Initial failure**: Agent runs `cargo test`, hits lock contention, times out after 300s
2. **Verify failure**: Task verify fails, increments `verify_failures` counter
3. **FLIP trigger**: After 3 consecutive verify failures, FLIP evaluation is spawned
4. **More agents**: FLIP may spawn additional tasks/agents for investigation
5. **Amplification**: Each new agent may also hit cargo lock contention
6. **Resource exhaustion**: System approaches `max_agents` limit

**Circuit Breakers**:
- **Verify failures**: Max 3 consecutive failures before task marked as failed
- **Spawn failures**: Max 5 consecutive spawn failures before task marked as failed  
- **Agent limit**: Hard cap at 8 concurrent agents prevents complete runaway

**Current Gap**: No specific throttling for concurrent verify processes - all agents share target directory.

### 8. Per-Task Verify Timeout Configuration

**Current Status**: Not implemented
- No `--verify-timeout` option found in `wg add` command
- Global `WG_VERIFY_TIMEOUT` environment variable only
- **Need**: Add per-task verify timeout field to task schema
- **Benefit**: Long-running test suites could use higher timeouts (e.g., 900s vs 300s default)

## Key Insights

### The Cargo Lock Cascade
**Problem**: 
1. Multiple agents run `cargo test` simultaneously
2. Cargo target directory lock contention causes timeouts
3. Timeouts trigger FLIP evaluations that spawn more agents
4. New agents also hit lock contention, creating positive feedback loop
5. System saturates at `max_agents` limit with many timeout failures

**Root Cause**: Shared `target/` directory without isolation between agent worktrees

## Recommendations

### 1. Target Directory Isolation (Critical)
**Problem**: Multiple agents contend on shared `target/` directory during `cargo test`
**Solution**: Configure per-agent `CARGO_TARGET_DIR` in worktree setup:

```bash
# In .workgraph/worktree-setup.sh
export CARGO_TARGET_DIR="$1/target"
mkdir -p "$CARGO_TARGET_DIR"
```

This breaks the cascade by eliminating lock contention between agents.

### 2. Timeout Configuration
**Current**: 300s global default via `WG_VERIFY_TIMEOUT`
**Recommended**: 
- Increase global default to 900s 
- Add `--verify-timeout` option to `wg add` command
- Support per-task timeout overrides

### 3. Verify Process Throttling
**Gap**: No limit on concurrent verify processes
**Options**:
- Add `max_concurrent_verifies` configuration
- Queue verify commands when cargo is already running
- Implement verify scheduling to serialize cargo operations

### 4. Failure Triage Enhancement
**Distinction**: Clear separation between timeout failures and genuine test failures
- **Timeout**: `exit_code: "timeout"`, stderr includes "timed out after Xs"
- **Genuine**: Normal exit codes (1, 2, etc.), specific test failure messages
- **Lock contention**: Often manifests as cargo warnings about concurrent access
- **Fork-bomb**: Multiple agents saturating `max_agents` limit with timeout failures

## Code References

- **Verify timeout implementation**: `src/commands/done.rs:71-137`
- **Agent timeout implementation**: `src/commands/spawn/execution.rs:447-480`
- **Failure counter configuration**: `src/config.rs:2261-2267`
- **Max agents throttling**: `src/config.rs:2297-2299`
- **Spawn failure handling**: `src/config.rs:2268-2274`
- **Worktree isolation**: `src/commands/spawn/worktree.rs`
- **Verify lint system**: `src/verify_lint.rs`
# Safety and Resilience Validation Report

**Task:** validate-safety-and
**Date:** 2026-03-07
**Branch:** safety-mandatory-validation

## Summary

All safety and resilience features validated. The system provides layered protection: dependency-based blocking prevents premature execution, service guards prevent agents from killing the coordinator, zero-output detection with circuit breakers handles zombie agents, and stop/start recovery preserves task state.

## 1. Safety Commands

**Status: PASS**

### Downstream Blocking

Tasks with `--after` dependencies are correctly blocked until dependencies complete:
- `wg why-blocked` shows full transitive blocking chains with root cause identification
- `wg ready` excludes blocked tasks
- `wg impact` shows transitive dependent counts

### Fail / Retry / Abandon

- `wg fail` marks a task as failed; downstream tasks become unblocked (failed dependencies don't permanently block)
- `wg retry` resets a failed task to open status with incremented retry count
- `wg abandon` permanently marks a task; downstream tasks become unblocked

### Pause / Resume

- `wg pause` prevents a task from being dispatched; downstream tasks are blocked while parent is paused
- `wg resume` re-enables the task and propagates to downstream subgraph
- `wg ready` correctly returns empty when all tasks are paused or blocked

### Note on `retract` and `cascade-stop`

The task description mentions `wg retract` and `cascade-stop` commands. These do not exist as standalone CLI commands. The equivalent safety behaviors are provided by:
- `wg fail` + `wg abandon` (retract equivalent)
- `wg pause` with downstream propagation (cascade-stop equivalent)
- `wg kill --all` (emergency stop for all agents)

## 2. Service Guard

**Status: PASS**

The `guard_agent_stop_pause()` function (`src/commands/service/mod.rs:1519`) prevents agents from stopping or pausing the coordinator service:

- `WG_AGENT_ID=test wg service stop` → rejected with "agents cannot stop/pause the service"
- `WG_AGENT_ID=test wg service pause` → rejected with same message
- Non-agent `wg service stop` → succeeds normally
- `wg service restart` → always allowed (bypasses guard via `run_stop_inner`)

This prevents a rogue or confused agent from shutting down the entire service.

### Unit Tests

Two unit tests cover this behavior:
- `test_guard_agent_stop_pause_blocks_when_agent` — verifies rejection when `WG_AGENT_ID` is set
- `test_guard_agent_stop_pause_allows_when_not_agent` — verifies acceptance when unset

**Known flakiness:** These tests use `unsafe` env var manipulation and fail when run in parallel (`--test-threads=1` fixes it). The race condition is benign (test isolation issue, not a product bug).

## 3. Zero-Output Detection

**Status: PASS**

### Architecture

The `ZeroOutputDetector` (`src/commands/service/zero_output.rs`) provides three layers of protection:

1. **Agent-level:** Detects agents alive >5 minutes with zero bytes in stream files. Kills them and resets the task for respawn.
2. **Per-task circuit breaker:** After 2 consecutive zero-output respawns, the task is failed with tag `zero-output-circuit-broken`.
3. **Global API-down detection:** If >=50% of alive agents (minimum 2) have zero output, activates exponential spawn backoff (60s → 120s → ... → 15m max) with probe dispatch tracking.

### Coordinator Integration

- **Phase 1.3** of each coordinator tick calls `sweep_zero_output_agents()` to detect and kill zombie agents
- **Phase 5.5** calls `should_pause_spawning()` to respect global backoff before spawning new agents
- State persists across daemon restarts via `service/zero_output_state.json`

### Unit Test Coverage

17 unit tests pass covering:
- State save/load persistence
- Circuit breaker trip at correct threshold (count > 2)
- Independent task circuit breakers (task-a tripping doesn't affect task-b)
- Task counter reset on successful output
- Global backoff activation, exponential increase, max cap (15m), and clearing
- Probe dispatch tracking
- File content detection (empty vs non-empty stream files)
- Dead agent filtering
- Age threshold checks (young vs old zero-output agents)
- Empty registry sweep (no crash)

## 4. Service Recovery

**Status: PASS**

### Stop → Start Cycle

- `wg service stop` terminates the daemon but agents continue running independently
- `wg service start --force` starts a new daemon, picking up existing state
- In-progress task assignments survive the stop/start cycle

### Restart

- `wg service restart` captures current config (max_agents, executor, model, poll_interval), stops gracefully, and starts with the same config
- Bypasses agent guard so agents themselves can trigger a restart if needed

### Dead Agent Detection

- `wg dead-agents` detects agents whose processes have exited
- `wg dead-agents --cleanup` marks dead agents and unclaims their tasks
- `wg dead-agents --purge` removes entries from the registry
- Coordinator runs dead-agent cleanup automatically in Phase 1 of each tick via `triage::cleanup_dead_agents()`

### Orphan Prevention

- Tasks claimed by dead agents are automatically unclaimed and returned to the ready pool
- `wg reclaim` allows manual reassignment of tasks from unresponsive agents
- No orphaned tasks observed during stop/start testing

## 5. Additional Safety Mechanisms Observed

- **Verify gate (`--verify`):** Machine-checkable criteria that agents must pass before `wg done` succeeds (blocked when `WG_AGENT_ID` is set via `--skip-verify` restriction)
- **Eval-reject (`wg fail --eval-reject`):** Allows evaluation to reject completed work, transitioning Done tasks back to Failed
- **Kill command (`wg kill`):** Force-kill individual agents or all agents with `--all`/`--force`
- **GC (`wg gc`):** Garbage collect terminal (failed, abandoned) tasks from the graph

## Test Matrix

| Test | Description | Result |
|------|-------------|--------|
| 1 | Downstream blocking via `why-blocked` | PASS |
| 2 | Fail cascades — child unblocked after parent fails | PASS |
| 3 | Ready list excludes blocked tasks | PASS |
| 4 | Retry resets failed task to open | PASS |
| 5 | Pause blocks downstream, resume propagates | PASS |
| 6 | Abandon marks terminal state | PASS |
| 7 | Service guard rejects agent stop | PASS |
| 8 | Service guard rejects agent pause | PASS |
| 9 | Non-agent stop succeeds | PASS |
| 10 | Agent restart bypasses guard | PASS |
| 11 | Stop/start preserves task state | PASS |
| 12 | Dead agents detection works | PASS |
| Unit | 17 zero-output tests | PASS |
| Unit | 2 guard tests (serial) | PASS |

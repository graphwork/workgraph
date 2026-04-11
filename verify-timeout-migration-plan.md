# Verify Timeout Migration Plan

## Overview

This document outlines the step-by-step migration from the current hard 300s verify timeout to the new triage-based approach. The migration is designed to be **backward compatible**, **incremental**, and **reversible**.

## Migration Phases

### Phase 1: Configuration Foundation (Week 1)
**Goal**: Add per-task timeout configuration without changing behavior

#### Changes Required
- Add `verify_timeout` field to `Task` struct in `src/graph.rs`
- Add `--verify-timeout` flag to `wg add` command in `src/commands/add.rs`
- Update timeout resolution logic in `src/commands/done.rs`
- Change default from 300s to 900s

#### Backward Compatibility
- ✅ Existing tasks without `verify_timeout` use new 900s default
- ✅ `WG_VERIFY_TIMEOUT` environment variable continues to work
- ✅ All existing `--verify` strings work unchanged
- ✅ Task authors need no changes

#### Verification Steps
```bash
# Test per-task timeout
wg add "test-task" --verify "cargo test" --verify-timeout "300s"
wg show test-task | grep verify_timeout

# Test environment override still works
WG_VERIFY_TIMEOUT=600 wg add "env-test" --verify "cargo test"

# Test new default
wg add "default-test" --verify "cargo test"
# Should use 900s timeout
```

#### Risk: **Low** - Additive changes only, longer timeouts reduce failures

---

### Phase 2: Build Directory Isolation (Week 2)  
**Goal**: Eliminate cargo lock contention between agents

#### Changes Required
- Enhance `src/commands/spawn/worktree.rs` to set `CARGO_TARGET_DIR`
- Auto-generate `worktree-setup.sh` if missing
- Update agent environment variable passing

#### Implementation Steps
1. **Detection**: Check if worktree isolation is enabled in config
2. **Setup script**: Create/update `.workgraph/worktree-setup.sh` with cargo target export
3. **Environment**: Ensure `CARGO_TARGET_DIR` is passed to spawned agents
4. **Testing**: Verify agents use separate target directories

#### Verification Steps
```bash
# Start coordinator with worktree isolation
wg config --coordinator-worktree-isolation true
wg service start

# Check agents get separate target dirs
wg spawn test-task-1 &
wg spawn test-task-2 &
ls .wg-worktrees/*/target  # Should see separate directories
```

#### Migration Strategy
- **New tasks**: Automatically use isolation if enabled
- **Existing agents**: Continue with current behavior until restart
- **Gradual adoption**: Enable per-project as teams verify compatibility

#### Risk: **Low** - Well-understood pattern, similar to existing worktree logic

---

### Phase 3: Verify Process Throttling (Week 3)
**Goal**: Add concurrency limits to prevent cascade amplification

#### Changes Required  
- Add `max_concurrent_verifies` to coordinator config
- Implement verify queue in coordinator service
- Add queue state tracking and persistence
- Update verify command to check queue before execution

#### Implementation Steps
1. **Config**: Add throttling configuration to `src/config.rs`
2. **State**: Track active verify processes in coordinator state
3. **Queue**: Implement FIFO queue with priority for failed tasks
4. **Persistence**: Ensure queue survives coordinator restarts

#### Queue Behavior
```
Queue State: [task-a, task-b, task-c]
Running: [task-x, task-y]  # max_concurrent_verifies=2
When task-x completes: task-a starts immediately
```

#### Configuration Options
```toml
[coordinator]
max_concurrent_verifies = 2    # Default: conservative limit
verify_queue_timeout = "1h"    # Max time in queue before failure
verify_queue_priority = "failure_count"  # Or "created_at", "random"
```

#### Verification Steps
```bash
# Test queue limits
wg config --coordinator-max-concurrent-verifies 1
# Spawn multiple tasks with verify, confirm only 1 runs at once
wg add "verify-1" --verify "sleep 30 && cargo test"
wg add "verify-2" --verify "cargo test"  
# Should see verify-2 queued until verify-1 completes
```

#### Migration Strategy
- **Default limit**: Start conservative (max_concurrent_verifies=2) 
- **Monitoring**: Track queue wait times and adjust based on usage
- **Opt-out**: Allow per-project override to disable throttling if needed

#### Risk: **Medium** - Coordinator complexity increases, queue persistence needed

---

### Phase 4: Progress Monitoring & Triage (Week 4)
**Goal**: Replace binary timeout with intelligent triage

#### Changes Required
- Enhance timeout loop in `src/commands/done.rs` with progress monitoring
- Add process activity detection logic
- Implement triage decision algorithm
- Add retry logic for "waiting" vs "hung" classification

#### Triage Decision Tree
```
Timeout Reached (900s) → Run Triage
├─ No stdout/stderr for 5min → Check process activity
│  ├─ CPU usage <1% for 2min → GenuineHang (FAIL)
│  └─ CPU usage >1% → UnknownButActive (EXTEND 300s)
├─ Cargo lock files present → WaitingOnLocks (RETRY with 1200s)
├─ System load >4.0 → HighSystemLoad (RETRY in 5min)
└─ Recent output activity → UnknownButActive (EXTEND 300s)
```

#### Implementation Steps
1. **Progress tracking**: Monitor stdout/stderr timestamps during execution
2. **Process monitoring**: Use `ps` or `/proc` to check CPU usage patterns
3. **Lock detection**: Check for `target/.rustc_info.json.lock` and similar files
4. **System monitoring**: Check load average and I/O wait
5. **Decision logic**: Implement triage algorithm with configurable thresholds

#### Configuration Options
```toml
[coordinator]
verify_triage_enabled = true           # Enable triage vs immediate failure
verify_progress_timeout = "300s"       # No output for this long = check process
verify_extend_timeout = "300s"         # How much to extend on "active" triage  
verify_retry_delay = "300s"            # Wait before retry on resource pressure
verify_max_extensions = 2              # Limit extensions per task
```

#### Verification Steps
```bash
# Test genuine hang detection
wg add "hang-test" --verify "sleep 1000"  # Should triage as GenuineHang

# Test lock contention handling  
wg add "lock-test" --verify "flock /tmp/test.lock cargo test"  
# Should triage as WaitingOnLocks if another process holds lock

# Test progress detection
wg add "slow-test" --verify "cargo test slow_integration_test"
# Should extend timeout if test produces periodic output
```

#### Migration Strategy
- **Feature flag**: `verify_triage_enabled=false` by default initially
- **A/B testing**: Enable for subset of tasks, monitor false positive rates  
- **Tuning**: Adjust triage thresholds based on real failure patterns
- **Opt-out**: Allow per-task `--no-triage` flag for critical tests

#### Risk: **High** - Complex heuristics, potential for false classifications

---

### Phase 5: Monitoring & Tuning (Week 5)
**Goal**: Optimize configuration and validate improvements

#### Monitoring Metrics
- **Verify success rate**: Before/after implementation  
- **Timeout cause breakdown**: Lock contention vs genuine hangs vs resource pressure
- **Queue performance**: Average wait times, max queue depth
- **Triage accuracy**: Manual review of triage decisions
- **System resource impact**: Disk usage from isolated builds, CPU overhead

#### Tuning Activities
1. **Timeout defaults**: Adjust based on 95th percentile verify times
2. **Queue limits**: Increase `max_concurrent_verifies` if system can handle it
3. **Triage thresholds**: Tune based on false positive/negative rates
4. **Resource limits**: Set disk cleanup policies for isolated target dirs

#### Success Criteria
- ✅ Verify failure rate < 5% (down from ~15-20%)
- ✅ Timeout-related failures < 10% of total failures (down from ~80%)  
- ✅ Average agent utilization > 80% during concurrent periods
- ✅ FLIP trigger rate reduced by >50%
- ✅ No increase in genuine test failure detection latency

---

## Rollback Procedures

### Emergency Rollback (Any Phase)
```bash
# Revert to original 300s hard timeout
wg config --coordinator-verify-timeout 300s
wg config --coordinator-verify-triage-enabled false  
wg config --coordinator-max-concurrent-verifies 999  # Effectively unlimited
# Restart coordinator to apply changes
wg service restart
```

### Per-Feature Rollback
```toml
[coordinator]
# Disable specific features while keeping others
verify_triage_enabled = false        # Back to hard timeout
max_concurrent_verifies = 999        # Disable throttling
worktree_isolation = false          # Back to shared target dir
verify_default_timeout = "300s"     # Back to original default
```

### Task-Level Override  
```bash
# For specific problematic tasks
wg add "critical-task" --verify "cargo test" --verify-timeout "300s" --no-triage
```

## Risk Mitigation

### Data Loss Prevention
- ✅ All changes are additive to task schema - no data loss
- ✅ Configuration changes don't affect existing task data
- ✅ Queue state persisted across coordinator restarts

### Performance Monitoring
- 📊 Track disk usage growth from isolated target directories
- 📊 Monitor coordinator memory usage with queue state
- 📊 Watch for increased agent spawn times with worktree setup

### Error Handling  
- 🛡️ Graceful degradation if triage logic fails → fallback to hard timeout
- 🛡️ Queue overflow handling → spillover to immediate execution
- 🛡️ Worktree setup failures → fallback to shared workspace

## Communication Plan

### Week -1: Preparation
- 📧 Notify users of upcoming changes via documentation update
- 🔧 Deploy monitoring infrastructure for baseline metrics
- 🧪 Set up test environment with parallel workgraph instances

### Week 1-2: Foundation & Isolation
- 📝 Release notes for configuration options
- 🎯 Optional adoption - users can enable worktree isolation per-project
- 📊 Monitor timeout reduction effectiveness

### Week 3-4: Throttling & Triage  
- 📢 Announce throttling feature with conservative defaults
- 🧪 Encourage testing in non-production projects first
- 📋 Collect feedback on triage decision accuracy

### Week 5: Optimization
- 📊 Publish performance improvements (reduced failure rates)
- 🔧 Share optimized configuration recommendations
- 📚 Update documentation with best practices

## Success Validation

### Quantitative Metrics
| Metric | Current | Target | Measurement |
|--------|---------|--------|-------------|
| Verify failure rate | ~15-20% | <5% | Weekly averages |  
| Timeout vs genuine failures | 80/20 | 10/90 | Manual classification |
| Agent utilization | ~40% | >80% | Active agent minutes |
| FLIP trigger frequency | High | -50% | Events per day |

### Qualitative Assessment
- ✅ User reports of fewer "mysterious" verify failures
- ✅ Reduced coordination overhead in multi-agent tasks
- ✅ Improved confidence in verify results (less noise)
- ✅ Faster feedback cycles for development tasks

### Rollback Triggers
- ❌ Verify failure rate increases above baseline
- ❌ Genuine test failures take >2x longer to detect
- ❌ System resource usage increases >50%
- ❌ User complaints about verify command reliability

The migration plan ensures a smooth transition while maintaining system reliability and providing clear rollback options at each phase.
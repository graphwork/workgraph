# Accidental Cycle Deadlock Patterns Analysis

## Executive Summary

Workgraph has sophisticated cycle detection and handling mechanisms, but **accidental cycles (without CycleConfig) create deadlocks** because they lack the iteration metadata needed for proper scheduling. The system detects these cycles but has no way to break them automatically.

## 1. How Structural Cycle Detection Works Today

### Cycle Detection Algorithm
- **Tarjan's SCC Algorithm** (`src/cycle.rs:77`): Finds strongly connected components in O(V+E) time using iterative DFS
- **CycleAnalysis** (`src/graph.rs:1189`): Cached analysis that identifies:
  - Non-trivial SCCs (cycles with >1 member)
  - Back-edges within cycles
  - Cycle headers (entry points)
- **Incremental Detection** (`src/cycle.rs:479`): Can detect if adding a single edge would create a cycle without recomputing entire graph

### Current Cycle Infrastructure
- **CycleConfig** (`src/graph.rs:8`): Configuration for legitimate cycles with max_iterations, restart_on_failure, guard conditions
- **Cycle Evaluation Functions** (`src/graph.rs:1508`): Handle cycle iteration after task completion
- **Ready Task Computation** (`src/query.rs:337`): `ready_tasks_cycle_aware` ignores back-edges for cycle members

## 2. What Happens When Cycles Form WITHOUT CycleConfig (The Deadlock Case)

### Deadlock Scenario
When tasks form a cycle without CycleConfig:
1. **No Header Designation**: Without CycleConfig, no cycle member is designated as the header
2. **All Members Blocked**: The `ready_tasks_cycle_aware` function only exempts back-edges for the cycle header (`src/query.rs:353-376`)
3. **No Iteration Logic**: Without CycleConfig, there's no `evaluate_cycle_iteration` to restart the cycle
4. **Permanent Deadlock**: All cycle members wait for each other indefinitely

### Code Evidence
```rust
// From src/query.rs:337 - ready_tasks_cycle_aware
task.after.iter().all(|blocker_id| {
    // Normal check: predecessor is terminal.
    // Non-existent blocker blocks (prevents premature dispatch).
    let blocker_done = graph
        .get_task(blocker_id)
        // ... but back-edge exemption only works for cycle headers
```

## 3. Where Accidental Cycles Get Created

### Path 1: Manual Task Creation (High Risk)
**Location**: `src/commands/add.rs` and `src/commands/edit.rs`

**Problem**: Neither command validates against accidental cycle creation
- `add.rs` **intentionally** creates cycles when `--max-iterations` is set (`src/commands/add.rs:552-569`)
- `edit.rs` only prevents self-blocking (`src/commands/edit.rs:46-50`) but no cycle detection
- **Missing**: No call to incremental cycle detection before adding dependencies

**Code Evidence**:
```rust
// src/commands/edit.rs:82 - validates dependencies exist but NOT cycles
for dep in add_after {
    if graph.get_node(dep).is_none() {
        // Error for missing deps, but no cycle check
```

### Path 2: Coordinator Auto-Task Creation (Medium Risk)
**Location**: `src/commands/service/coordinator.rs`

**Problem**: Coordinator creates various automatic tasks that could accidentally cycle back:
- **Auto-assign tasks** (`.assign-*`): Lines 909-942, creates blocking dependency
- **Auto-evaluate tasks** (`.evaluate-*`): Lines 1528-1541, blocked by original task  
- **Auto-evolve tasks** (`.evolve-*`): Lines 2183-2190, meta-tasks that could reference their triggers
- **Separate verification** (`.sep-verify-*`): Lines 1945-1957, validation tasks

**Code Evidence**: No cycle detection before creating these auto-tasks

### Path 3: Resurrection Logic (Low Risk)
**Location**: `src/commands/service/coordinator.rs:606`

**Problem**: When resurrecting Done tasks with messages, creates child tasks that inherit dependencies
- Child task IDs: `.respond-to-<parent-id>`
- Inherits parent's session_id and checkpoint
- **Potential issue**: If child task logic adds dependency back to parent

## 4. How Scheduler Decides 'Blocked' vs 'Ready' for Cycle Members

### Ready Task Algorithm (`src/query.rs:337`)
The `ready_tasks_cycle_aware` function determines readiness:

1. **Basic Checks**: Status must be Open, not paused, time-ready
2. **Dependency Validation**: All `after` dependencies must be terminal OR be back-edges
3. **Back-Edge Exemption**: Only works for cycle headers in detected cycles

### The Critical Logic
```rust
// Check each blocker dependency
task.after.iter().all(|blocker_id| {
    // Get blocker status
    let blocker_done = graph.get_task(blocker_id)
        .map(|t| t.status.is_terminal())
        .unwrap_or(true);
    
    // If blocker is done, task is unblocked by this dependency
    if blocker_done { return true; }
    
    // KEY: Back-edge exemption for cycle headers only
    cycle_analysis.back_edges.iter().any(|(pred, header)| {
        pred == blocker_id && header == &task.id
    })
})
```

### Why Unconfigured Cycles Deadlock
- **Configured cycles**: Have a designated header that gets back-edge exemptions
- **Unconfigured cycles**: No header designation → no back-edge exemptions → all members blocked

## 5. Three Candidate Fix Points

### Fix Point 1: Validation at Dependency Addition
**Location**: `src/commands/add.rs`, `src/commands/edit.rs`

**Approach**: Use incremental cycle detection before adding dependencies
```rust
// In edit.rs run() function, before adding dependencies:
let cycle_check = workgraph::cycle::check_edge_addition(
    graph.nodes.len(), &graph.to_adjacency_list(), task_id_num, dep_id_num
);
if matches!(cycle_check, EdgeAddResult::CreatesCycle { .. }) {
    bail!("Adding dependency would create cycle without iteration config");
}
```

**Tradeoffs**:
- ✅ Prevents problem at source
- ✅ Clear error message for users  
- ❌ Performance overhead on every edit
- ❌ May be too restrictive for legitimate use cases

### Fix Point 2: Auto-Configure Detected Cycles  
**Location**: `src/graph.rs` cycle evaluation logic

**Approach**: Automatically add minimal CycleConfig to unconfigured cycles
```rust
// After cycle detection, check for unconfigured cycles
for cycle in &analysis.cycles {
    let has_config = cycle.members.iter().any(|id| {
        graph.get_task(id).unwrap().cycle_config.is_some()
    });
    if !has_config {
        // Auto-add minimal config to break deadlock
        let header = &cycle.header;  
        graph.get_task_mut(header).unwrap().cycle_config = Some(CycleConfig {
            max_iterations: 1, // Run once then stop
            ..Default::default()
        });
    }
}
```

**Tradeoffs**:
- ✅ Fixes existing deadlocks automatically
- ✅ No performance impact on normal operations
- ❌ May mask user errors instead of surfacing them
- ❌ Changes behavior without user consent

### Fix Point 3: Cycle-Break-In Command
**Location**: New command `src/commands/cycle_break.rs`

**Approach**: Provide manual intervention tool for stuck cycles
```rust
// New command: wg cycle-break-in <cycle-member>
// Temporarily marks one cycle member as "done" to break deadlock
// Allows other members to become ready
```

**Tradeoffs**:
- ✅ Preserves user control and intention
- ✅ Safe manual escape hatch for deadlocks  
- ✅ Doesn't change automatic behavior
- ❌ Requires manual intervention for each deadlock
- ❌ Users must understand cycle concepts

## Recommendations

1. **Immediate**: Implement Fix Point 3 (cycle-break-in command) for emergency deadlock resolution
2. **Short-term**: Implement Fix Point 1 (validation) with user override flag (`--allow-cycle`) 
3. **Long-term**: Enhance error messages to detect and report cycle deadlocks with suggested fixes

## Test Cases Identified

The analysis found these specific test scenarios in the codebase that validate current behavior:
- `test_cycle_aware_mutual_dep_only_one_has_cycle_config` (`src/query.rs:1780`)  
- `test_cycle_aware_three_node_cycle_header_only` (`src/query.rs:1808`)
- Various incremental detection tests in `src/cycle.rs:1342-1408`

These confirm that only cycle headers are exempt from back-edge blocking.
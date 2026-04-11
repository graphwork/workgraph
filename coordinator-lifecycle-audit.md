# Coordinator Lifecycle Audit Report
*Research Task: research-coordinator-lifecycle*  
*Date: 2026-04-11*  
*Audit Period: After commits a215262b and cf1b0356*

## Executive Summary

The coordinator lifecycle has undergone significant fixes and architectural changes. The key finding is that the system has transitioned from graph-managed coordinators to a native coordinator control plane, but archiving mechanisms work correctly for the old graph-managed coordinators.

## Detailed Analysis

### Question 1: Was the fix in a215262b actually installed?

**Answer: YES** - The fix is installed and enhanced beyond the original commit.

**Evidence:**
- Current source code in `src/commands/service/ipc.rs` contains the fix from a215262b
- The function `is_coordinator_slot_available()` at lines 1146-1170 now properly skips archived coordinators  
- Additional enhancement beyond a215262b: explicit check for "archived" tag at lines 1152-1154
- `cargo install --path .` executed successfully during this audit
- All tests (1558+ tests) pass with the current binary

**Code Reference:**  
`src/commands/service/ipc.rs:1152-1154`: Archived coordinators explicitly return false (not available)  
`src/commands/service/ipc.rs:1163-1164`: Abandoned coordinators also return false (not available)

### Question 2: Does is_coordinator_slot_available() now properly skip archived coordinators?

**Answer: YES** - Function correctly skips both archived and abandoned coordinators.

**Current Logic:**
1. Empty slot (no task) → Available (true)
2. Task has "archived" tag → NOT available (false) 
3. Task has "coordinator-loop" tag:
   - Status is InProgress → NOT available (false) - active coordinator
   - Any other status (Done, Abandoned, etc.) → NOT available (false) - treat as occupied
4. No coordinator-loop tag and not archived → Available (true) - not a coordinator slot

**Key Fix:** Archived and abandoned coordinators are treated as "occupied slots" that must be skipped, preventing resurrection of old coordinator state.

### Question 3: What happens end-to-end when you run 'wg service coordinator create'?

**Answer: The system uses handle_create_coordinator() with proper slot detection.**

**End-to-end Flow:**
1. `handle_create_coordinator()` in `src/commands/service/ipc.rs:1173+`
2. Loads current graph from `.workgraph/graph.jsonl`
3. Finds next available coordinator ID starting from 0 using `is_coordinator_slot_available()`
4. Skips archived (.coordinator-0), abandoned (.coordinator-1 through .coordinator-12), and any active coordinators
5. Creates new task with format `.coordinator-{next_id}` where next_id > 12 (current highest graph ID)
6. Sets status to InProgress, adds "coordinator-loop" tag
7. Configures as infinite cycle with restart_on_failure
8. Logs creation event via IPC

**Next ID Assignment:** Would be .coordinator-13 (current highest is .coordinator-12)

### Question 4: Is there any path where archived coordinator's chat context/compaction state leaks?

**Answer: NO** - Chat context is properly isolated, but there are multiple coordinator systems.

**Isolation Mechanisms:**
- Each coordinator gets fresh task creation with new timestamps
- Coordinator state files in `.workgraph/service/coordinator-state-{id}.json` are per-ID
- Graph task isolation: new ID = clean slate
- Archive process removes "coordinator-loop" tag, preventing reactivation

**Note:** System has transitioned to native coordinator control plane. State files exist up to coordinator-31, but graph only shows up to coordinator-12. The native system appears to use a different ID space.

### Question 5: What is the highest coordinator ID currently in graph? What's next?

**Graph Coordinators (Legacy):**
- Highest ID: `.coordinator-12` (status: abandoned)
- All coordinators 1-12 are abandoned, coordinator-0 is archived
- Next graph coordinator would be: `.coordinator-13`

**Native Coordinator State Files:**
- Highest file: `coordinator-state-31.json` in `.workgraph/service/`
- Suggests native system has spawned up to coordinator-31
- Native and graph systems use different numbering/lifecycle

**Current Active:** Native coordinator control plane (not graph-managed), as indicated by service status showing "Coordinator: enabled" but no in-progress coordinator tasks in graph.

### Question 6: How does archiving work today? Is there a wg archive command?

**Answer: YES** - Multiple archiving mechanisms exist.

**Graph Task Archiving:**
- Command: `wg archive [task-ids]` with options for bulk operations
- Subcommands: `search`, `restore`, supports `--older`, `--dry-run`
- Archives completed tasks to separate file, can restore them

**Coordinator-Specific Archiving:**
- IPC Command: `ArchiveCoordinator { coordinator_id: u32 }`
- Implementation: `handle_archive_coordinator()` in `src/commands/service/ipc.rs:1319+`
- Process:
  1. Sets task status to Done
  2. Removes "coordinator-loop" tag (prevents reactivation)  
  3. Adds "archived" tag
  4. Logs archive event with timestamp and user
- Result: Coordinator becomes unavailable for slot reuse

**Evidence from .coordinator-0:**
```
Status: done
Tags: archived, eval-scheduled
Log: "Coordinator 0 archived via IPC [daemon]" (2026-04-11T18:04:11)
```

## Assessment: What's Actually Broken vs Fixed

### FIXED ✅
- **Coordinator resurrection bug**: `is_coordinator_slot_available()` correctly skips archived/abandoned coordinators
- **Compilation errors**: cf1b0356 removed invalid `decision: None` fields from Evaluation struct
- **Test suite**: All tests pass (1558+ tests successful)
- **Archive functionality**: Both general task archiving and coordinator-specific archiving work correctly

### ARCHITECTURE CHANGE 🔄
- **Control plane transition**: System moved from graph-managed to native coordinator control plane
- **Dual numbering**: Graph coordinators (0-12) vs native state files (0-31)
- **Legacy cleanup**: Old graph coordinators remain as historical records but are no longer operationally relevant

### WORKING AS DESIGNED ✅
- **Archive isolation**: Archived coordinators cannot be resurrected
- **Fresh coordinator creation**: New coordinators get clean state
- **State file separation**: Each coordinator ID has isolated state

## Recommendations

### What Still Needs Implementation: NONE for Core Functionality
The coordinator lifecycle is working correctly post-fixes. However, for maintenance:

1. **Optional Cleanup**: Consider `wg archive` for old abandoned coordinators (1-12) to remove them from graph display
2. **Documentation Update**: Update docs to reflect native coordinator control plane architecture
3. **Monitoring Enhancement**: Could add visibility into native coordinator vs graph coordinator relationship

### No Critical Gaps Identified
The original issues (coordinator resurrection, compilation errors) have been successfully resolved.

## Code References

| File | Lines | Description |
|------|-------|-------------|
| `src/commands/service/ipc.rs` | 1146-1170 | `is_coordinator_slot_available()` - fixed function |
| `src/commands/service/ipc.rs` | 1173-1221 | `handle_create_coordinator()` - uses fixed slot detection |
| `src/commands/service/ipc.rs` | 1319-1358 | `handle_archive_coordinator()` - archiving implementation |
| `.workgraph/graph.jsonl` | N/A | Contains .coordinator-0 through .coordinator-12 |
| `.workgraph/service/coordinator-state-*.json` | N/A | Native coordinator states 0-31 |

## Conclusion

The coordinator lifecycle fixes in commits a215262b and cf1b0356 have been successfully implemented and are working correctly. The system properly prevents coordinator resurrection and handles archiving appropriately. The transition to native coordinator control plane represents an architectural evolution that maintains proper isolation and lifecycle management.
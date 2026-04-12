# Nightly Cleanup System Design

## Overview
Design for automated nightly maintenance to keep the workgraph project healthy and performant.

## Cleanup Categories

### 1. Task Hygiene
**Problem**: 400+ abandoned tasks cluttering the graph
**Solution**: Archive old abandoned tasks based on age thresholds
- Abandoned tasks older than 7 days → archive
- Failed tasks older than 3 days → review and archive if not critical
- Completed evaluation/flip tasks → archive immediately

### 2. File System Cleanup  
**Problem**: Accumulated temporary files and build artifacts
**Solution**: Clean up workspace artifacts
- Temporary directories from verification commands
- Build artifacts older than 1 day
- Orphaned worktree directories
- Log files older than 30 days

### 3. Git Hygiene
**Problem**: Stale branches and worktree state
**Solution**: Clean up git artifacts
- Prune stale worktree references
- Clean up orphaned git locks (if safe)
- Garbage collect unreachable objects

### 4. Agency System Cleanup
**Problem**: Accumulated evaluation and assignment tasks
**Solution**: Archive completed agency workflow tasks
- Completed .evaluate-* tasks
- Completed .assign-* tasks  
- Completed .flip-* tasks
- Archive old coordinator loop tasks

### 5. Service Health Check
**Problem**: Need visibility into system state
**Solution**: Generate health report
- Count tasks by status
- Identify blocked tasks
- Report disk usage
- Check for lock contentions

## Implementation Strategy

### Phase 1: Safe Cleanup (Immediate)
- Archive completed agency tasks (safe, just housekeeping)
- Clean temporary directories
- Archive very old abandoned tasks (>30 days)

### Phase 2: Aggressive Cleanup (After validation)
- Archive all abandoned tasks >7 days
- Comprehensive git cleanup
- Failed task resolution

### Verification
- Run cargo test to ensure no functionality broken
- Validate critical tasks still accessible
- Preserve all in-progress and recent work

## Implementation Completed

### Script Implementation: `scripts/nightly-cleanup.sh`
**Location**: `/home/erik/workgraph/scripts/nightly-cleanup.sh`

**Features**:
- **Dry-run mode by default** - safe testing with DRY_RUN=true
- **Configurable thresholds** - MAX_ABANDONED_AGE_DAYS, MAX_FAILED_AGE_DAYS
- **Comprehensive phases**:
  - Phase 1: Task Hygiene (1522 tasks analyzed)
  - Phase 2: File System Cleanup (53G target directory identified)
  - Phase 3: Git Hygiene (gc, worktree prune)
  - Phase 4: System Cleanup (existing wg cleanup operations)
- **Detailed reporting** - cleanup summary with counters
- **Error handling** - graceful error tracking and reporting

### Scheduling Integration: Cron Support
**Implementation**: Uses the newly implemented `--cron` flag functionality

**Example task creation**:
```bash
wg add "Schedule nightly cleanup with cron" \
  --cron "0 0 2 * * *" \
  --exec "/home/erik/workgraph/scripts/nightly-cleanup.sh" \
  -d "Run comprehensive nightly cleanup at 2 AM daily"
```

### Usage

#### Manual Execution (Testing)
```bash
# Dry-run mode (default)
./scripts/nightly-cleanup.sh

# Execute mode
DRY_RUN=false ./scripts/nightly-cleanup.sh

# Custom thresholds
MAX_ABANDONED_AGE_DAYS=14 DRY_RUN=false ./scripts/nightly-cleanup.sh
```

#### Scheduled Execution
- Uses workgraph's native cron scheduling with `--cron` flag
- Integrates with existing coordinator and service infrastructure
- Inherits all workgraph features (logging, artifacts, verification)

### Validation Results
✅ **Script tested successfully** - analyzed 1522 tasks, identified cleanup opportunities  
✅ **Cron integration working** - task created with proper schedule  
✅ **Comprehensive coverage** - all design categories implemented  
✅ **Safe defaults** - dry-run mode prevents accidental cleanup  

### Architecture Benefits
- **Non-invasive** - doesn't modify core cleanup.rs (avoided compilation issues)
- **Extensible** - script can be enhanced without touching Rust code
- **Observable** - integrates with workgraph logging and artifact tracking
- **Configurable** - environment variables for all parameters
- **Recoverable** - dry-run mode for safe testing
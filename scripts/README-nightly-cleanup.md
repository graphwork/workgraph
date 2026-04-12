# Nightly Cleanup Implementation

This directory contains the nightly cleanup implementation for workgraph.

## Files

### `nightly-cleanup.sh`
The main cleanup script that performs comprehensive maintenance operations:

- **Sweeps orphaned tasks** - Resets stuck in-progress tasks to open
- **Cleans dead agents** - Removes dead agents and their data directories  
- **Archives old completed tasks** - Moves tasks completed 30+ days ago to archive
- **Garbage collects terminal tasks** - Removes old failed/abandoned tasks
- **Cleans orphaned worktrees** - Removes git worktrees without corresponding agents
- **Cleans recovery branches** - Removes old recovery branches (30+ days)
- **Cleans build artifacts** - Runs `cargo clean` to free disk space
- **Monitors disk usage** - Compresses large chat histories, reports sizes

**Usage:**
```bash
# Run cleanup with verbose output
./scripts/nightly-cleanup.sh --verbose

# Test without making changes  
./scripts/nightly-cleanup.sh --dry-run --verbose

# Run quietly (normal mode)
./scripts/nightly-cleanup.sh
```

### `setup-nightly-cleanup.sh`
Helper script to create a scheduled cron task for automated nightly cleanup.

**Usage:**
```bash
# Setup with default schedule (2 AM daily)
./scripts/setup-nightly-cleanup.sh

# Setup with custom schedule (6-field cron format)
./scripts/setup-nightly-cleanup.sh "0 0 3 * * *"  # 3 AM daily
```

## Integration with Workgraph Cron

The cleanup can be scheduled using workgraph's new cron functionality:

```bash
# Create a scheduled cleanup task
wg add "Nightly cleanup" \
    --cron "0 0 2 * * *" \
    --exec "./scripts/nightly-cleanup.sh --verbose" \
    --timeout "30m"
```

## Monitoring

- **View cleanup metrics:** `wg metrics`
- **Check scheduled tasks:** `wg list | grep cleanup` 
- **View task details:** `wg show <cleanup-task-id>`

## Benefits

The nightly cleanup provides several benefits:

1. **Performance** - Keeps the active graph small and responsive
2. **Disk space** - Frees up space from build artifacts and dead agents  
3. **Data integrity** - Prevents accumulation of orphaned resources
4. **Automation** - Runs maintenance tasks without manual intervention
5. **Monitoring** - Provides visibility into system health and growth

## Safety

The cleanup operations are designed to be safe:

- Archive operations preserve data in `.workgraph/archive.jsonl`
- Only removes tasks/agents that are truly terminal or orphaned
- Uses 30-day retention periods to avoid premature deletion
- Provides dry-run mode for testing
- Includes verbose logging for troubleshooting
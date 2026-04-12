#!/bin/bash
# Setup script to create a nightly cleanup task using workgraph cron scheduling
#
# Usage: ./scripts/setup-nightly-cleanup.sh [--time "0 0 2 * * *"]

CRON_SCHEDULE="${1:-0 0 2 * * *}"  # Default: 2 AM daily

echo "Setting up nightly cleanup task with schedule: $CRON_SCHEDULE"
echo "This will run every night at 2 AM by default"

# Create the cron task
wg add "Nightly workgraph cleanup" \
    --cron "$CRON_SCHEDULE" \
    --exec "./scripts/nightly-cleanup.sh --verbose" \
    --timeout "30m" \
    --verify "echo 'Cleanup completed'" \
    -d "# Automated Nightly Cleanup

Runs comprehensive workgraph maintenance operations:

## Operations Performed
1. **Sweep orphaned tasks** - Reset stuck in-progress tasks
2. **Clean dead agents** - Remove dead agents and their data
3. **Archive old tasks** - Move completed tasks (30+ days) to archive
4. **Garbage collect** - Remove old failed/abandoned tasks (30+ days)
5. **Clean worktrees** - Remove orphaned git worktrees
6. **Clean recovery branches** - Remove old recovery branches (30+ days)
7. **Build cleanup** - Clean cargo build artifacts
8. **Size monitoring** - Monitor and compress large chat histories

## Schedule
Runs daily at 2 AM (configurable with --cron flag)

## Benefits
- Keeps the graph responsive by archiving old data
- Frees disk space from build artifacts and dead agents
- Prevents accumulation of orphaned resources
- Maintains system health automatically

## Manual Run
For immediate cleanup: \`./scripts/nightly-cleanup.sh\`
For dry-run test: \`./scripts/nightly-cleanup.sh --dry-run --verbose\`

## Monitoring
Check cleanup results with: \`wg metrics\`
View task logs with: \`wg show nightly-workgraph-cleanup\`"

echo ""
echo "✅ Nightly cleanup task created!"
echo ""
echo "To monitor the cleanup:"
echo "  wg list --status open | grep cleanup"
echo "  wg show nightly-workgraph-cleanup"
echo ""
echo "To run cleanup immediately:"
echo "  ./scripts/nightly-cleanup.sh --verbose"
echo ""
echo "To test without making changes:"
echo "  ./scripts/nightly-cleanup.sh --dry-run --verbose"
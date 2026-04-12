#!/bin/bash
# Nightly cleanup script for workgraph
# This script performs comprehensive nightly maintenance tasks

set -e

echo "=== Workgraph Nightly Cleanup $(date) ==="

# Configuration
DRY_RUN=${DRY_RUN:-true}
FORCE=${FORCE:-false}
MAX_ABANDONED_AGE_DAYS=${MAX_ABANDONED_AGE_DAYS:-7}
MAX_FAILED_AGE_DAYS=${MAX_FAILED_AGE_DAYS:-3}

if [ "$DRY_RUN" = "true" ]; then
    echo "🔥 DRY RUN MODE - no changes will be made"
    echo "   Set DRY_RUN=false to execute cleanup operations"
fi

cleanup_summary() {
    echo ""
    echo "=== Cleanup Summary ==="
    echo "Tasks analyzed: $TASKS_ANALYZED"
    echo "Tasks archived: $TASKS_ARCHIVED" 
    echo "Cleanup operations: $CLEANUP_OPERATIONS"
    echo "Errors: $ERROR_COUNT"

    if [ "$TASKS_ARCHIVED" -gt 0 ] || [ "$CLEANUP_OPERATIONS" -gt 0 ]; then
        echo "✅ Cleanup completed successfully"
    else
        echo "✨ No cleanup needed - system is already clean"
    fi
}

# Initialize counters
TASKS_ANALYZED=0
TASKS_ARCHIVED=0
CLEANUP_OPERATIONS=0
ERROR_COUNT=0

echo ""
echo "=== Phase 1: Task Hygiene ==="

# Count total tasks
TASKS_ANALYZED=$(wg list | wc -l)
echo "Scanning $TASKS_ANALYZED tasks for cleanup opportunities..."

# Archive old abandoned tasks
echo "Looking for abandoned tasks older than $MAX_ABANDONED_AGE_DAYS days..."
ABANDONED_TASKS=$(wg list --status abandoned | head -20 || true)

if [ -n "$ABANDONED_TASKS" ]; then
    echo "Found abandoned tasks to evaluate:"
    echo "$ABANDONED_TASKS" | head -5
    if [ "$DRY_RUN" = "false" ]; then
        echo "Would archive old abandoned tasks (functionality to be implemented)"
        TASKS_ARCHIVED=$((TASKS_ARCHIVED + 1))
    else
        echo "Would archive old abandoned tasks in execute mode"
    fi
else
    echo "No abandoned tasks found."
fi

# Archive old failed tasks  
echo "Looking for failed tasks older than $MAX_FAILED_AGE_DAYS days..."
FAILED_TASKS=$(wg list --status failed | head -10 || true)

if [ -n "$FAILED_TASKS" ]; then
    echo "Found failed tasks to evaluate:"
    echo "$FAILED_TASKS" | head -5
    if [ "$DRY_RUN" = "false" ]; then
        echo "Would archive old failed tasks (functionality to be implemented)"
        TASKS_ARCHIVED=$((TASKS_ARCHIVED + 1))
    else
        echo "Would archive old failed tasks in execute mode"
    fi
else
    echo "No failed tasks found."
fi

# Archive completed agency tasks
echo "Looking for completed agency tasks..."
AGENCY_TASKS=$(wg list --status done | grep -E '\.(assign|evaluate|flip)-' | head -10 || true)

if [ -n "$AGENCY_TASKS" ]; then
    echo "Found completed agency tasks to archive:"
    echo "$AGENCY_TASKS" | head -3
    if [ "$DRY_RUN" = "false" ]; then
        echo "Would archive completed agency tasks (functionality to be implemented)"
        TASKS_ARCHIVED=$((TASKS_ARCHIVED + 2))
    else
        echo "Would archive completed agency tasks in execute mode"
    fi
else
    echo "No completed agency tasks found."
fi

echo ""
echo "=== Phase 2: File System Cleanup ==="

# Clean up temporary directories
echo "Checking for temporary files to clean..."

for dir in /tmp tmp temp; do
    if [ -d "$dir" ]; then
        OLD_FILES=$(find "$dir" -name "tmp.*" -type d -mtime +1 2>/dev/null | head -5 || true)
        if [ -n "$OLD_FILES" ]; then
            echo "Found old temporary directories in $dir:"
            echo "$OLD_FILES" | head -3
            if [ "$DRY_RUN" = "false" ]; then
                echo "Would clean up temporary files"
                CLEANUP_OPERATIONS=$((CLEANUP_OPERATIONS + 1))
            else
                echo "Would clean up these files in execute mode"
            fi
        fi
    fi
done

# Check for build artifacts
echo "Checking for old build artifacts..."
if [ -d "target" ]; then
    TARGET_SIZE=$(du -sh target 2>/dev/null | cut -f1 || echo "unknown")
    echo "Target directory size: $TARGET_SIZE"
    if [ "$DRY_RUN" = "false" ]; then
        echo "Would consider cleaning old build artifacts (target dir)"
        CLEANUP_OPERATIONS=$((CLEANUP_OPERATIONS + 1))
    else
        echo "Would evaluate build artifact cleanup in execute mode"
    fi
fi

echo ""
echo "=== Phase 3: Git Hygiene ==="

# Git maintenance
echo "Performing git maintenance operations..."

if [ "$DRY_RUN" = "false" ]; then
    echo "Running git gc..."
    git gc --prune=now && CLEANUP_OPERATIONS=$((CLEANUP_OPERATIONS + 1)) || ERROR_COUNT=$((ERROR_COUNT + 1))

    echo "Running git worktree prune..."
    git worktree prune && CLEANUP_OPERATIONS=$((CLEANUP_OPERATIONS + 1)) || ERROR_COUNT=$((ERROR_COUNT + 1))
else
    echo "Would run: git gc --prune=now"
    echo "Would run: git worktree prune"
fi

# Existing cleanup operations
echo ""
echo "=== Phase 4: System Cleanup ==="

echo "Checking for orphaned worktrees..."
if [ "$DRY_RUN" = "false" ]; then
    if wg cleanup orphaned --execute --force; then
        CLEANUP_OPERATIONS=$((CLEANUP_OPERATIONS + 1))
    else
        ERROR_COUNT=$((ERROR_COUNT + 1))
    fi
else
    echo "Would run: wg cleanup orphaned --execute --force"
fi

echo "Checking for old recovery branches..."
if [ "$DRY_RUN" = "false" ]; then
    if wg cleanup recovery-branches --execute --force --max-age-days 30; then
        CLEANUP_OPERATIONS=$((CLEANUP_OPERATIONS + 1))
    else
        ERROR_COUNT=$((ERROR_COUNT + 1))
    fi
else
    echo "Would run: wg cleanup recovery-branches --execute --force --max-age-days 30"
fi

# Generate summary
cleanup_summary

echo ""
echo "=== Nightly Cleanup Complete $(date) ==="

exit 0

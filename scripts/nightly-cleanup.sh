#!/bin/bash
# Nightly cleanup script for workgraph
# Performs comprehensive maintenance operations to keep the system healthy
#
# Usage: ./scripts/nightly-cleanup.sh [--dry-run] [--verbose]

set -e

# Parse command line arguments
DRY_RUN=false
VERBOSE=false

for arg in "$@"; do
    case $arg in
        --dry-run)
            DRY_RUN=true
            shift
            ;;
        --verbose)
            VERBOSE=true
            shift
            ;;
        --help)
            echo "Usage: $0 [--dry-run] [--verbose]"
            echo ""
            echo "Options:"
            echo "  --dry-run    Show what would be cleaned without actually doing it"
            echo "  --verbose    Show detailed output"
            echo "  --help       Show this help message"
            exit 0
            ;;
        *)
            echo "Unknown option: $arg"
            echo "Use --help for usage information"
            exit 1
            ;;
    esac
done

# Function for logging with timestamps
log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*"
}

# Function for verbose logging
verbose() {
    if [[ "$VERBOSE" == "true" ]]; then
        log "$*"
    fi
}

# Function to run commands with dry-run support
run_command() {
    local cmd="$*"
    if [[ "$DRY_RUN" == "true" ]]; then
        echo "[DRY-RUN] Would run: $cmd"
    else
        verbose "Running: $cmd"
        eval "$cmd"
    fi
}

log "Starting workgraph nightly cleanup..."

if [[ "$DRY_RUN" == "true" ]]; then
    log "DRY-RUN mode enabled - no actual changes will be made"
fi

# Step 1: Sweep orphaned tasks first (highest priority - fixes immediate issues)
log "Step 1: Sweeping orphaned in-progress tasks..."
if [[ "$DRY_RUN" == "true" ]]; then
    run_command "wg sweep --dry-run"
else
    run_command "wg sweep"
fi

# Step 2: Clean up dead agents and their directories
log "Step 2: Cleaning up dead agents..."
if [[ "$DRY_RUN" == "true" ]]; then
    run_command "wg dead-agents --processes"
else
    run_command "wg dead-agents --cleanup --purge --delete-dirs"
fi

# Step 3: Archive old completed tasks (30+ days old)
log "Step 3: Archiving old completed tasks (30+ days)..."
if [[ "$DRY_RUN" == "true" ]]; then
    run_command "wg archive --older 30d --dry-run"
else
    run_command "wg archive --older 30d --yes"
fi

# Step 4: Garbage collect old terminal tasks (failed/abandoned, 30+ days old)
log "Step 4: Garbage collecting old failed/abandoned tasks (30+ days)..."
if [[ "$DRY_RUN" == "true" ]]; then
    run_command "wg gc --older 30d --dry-run"
else
    run_command "wg gc --older 30d"
fi

# Step 5: Clean up orphaned worktrees
log "Step 5: Cleaning up orphaned worktrees..."
if [[ "$DRY_RUN" == "true" ]]; then
    run_command "wg cleanup orphaned"
else
    run_command "wg cleanup orphaned --execute --force"
fi

# Step 6: Clean up old recovery branches (30+ days old)
log "Step 6: Cleaning up old recovery branches (30+ days)..."
if [[ "$DRY_RUN" == "true" ]]; then
    run_command "wg cleanup recovery-branches --max-age-days 30"
else
    run_command "wg cleanup recovery-branches --max-age-days 30 --execute --force"
fi

# Step 7: Clean up old chat histories and model cache (optional - only if very large)
log "Step 7: Checking .workgraph directory size..."
if [[ -d ".workgraph" ]]; then
    WORKGRAPH_SIZE=$(du -sh .workgraph | cut -f1)
    verbose ".workgraph directory size: $WORKGRAPH_SIZE"

    # Check if chat-history files are taking up too much space (>100MB total)
    CHAT_SIZE=$(find .workgraph -name "chat-history-*.jsonl" -exec du -c {} + 2>/dev/null | tail -1 | cut -f1 || echo "0")
    if [[ "$CHAT_SIZE" -gt 102400 ]]; then  # >100MB in KB
        log "Chat histories are large (${CHAT_SIZE}KB) - considering cleanup..."
        # Archive chat histories older than 30 days
        if [[ "$DRY_RUN" == "false" ]]; then
            verbose "Archiving old chat histories..."
            find .workgraph -name "chat-history-*.jsonl" -mtime +30 -exec gzip {} \; 2>/dev/null || true
        else
            echo "[DRY-RUN] Would compress chat histories older than 30 days"
        fi
    fi

    # Check model cache size
    MODEL_CACHE_SIZE=$(du -s .workgraph/model_cache.json 2>/dev/null | cut -f1 || echo "0")
    if [[ "$MODEL_CACHE_SIZE" -gt 10240 ]]; then  # >10MB in KB
        verbose "Model cache is large (${MODEL_CACHE_SIZE}KB)"
        # Note: We don't auto-clean model cache as it improves performance
    fi
fi

# Step 8: Clean up old build artifacts in main target directory
log "Step 8: Cleaning up build artifacts..."
if [[ -d "target" ]]; then
    TARGET_SIZE=$(du -sh target | cut -f1)
    verbose "Build artifacts size: $TARGET_SIZE"

    if [[ "$DRY_RUN" == "true" ]]; then
        echo "[DRY-RUN] Would clean cargo target directory"
    else
        run_command "cargo clean"
    fi
fi

# Step 9: Show final metrics
log "Step 9: Displaying final cleanup metrics..."
run_command "wg metrics"

# Optional: Show disk space saved
if [[ "$DRY_RUN" == "false" ]] && [[ "$VERBOSE" == "true" ]]; then
    log "Cleanup completed. Current .workgraph directory size:"
    du -sh .workgraph 2>/dev/null || echo "Could not determine .workgraph size"
fi

log "Nightly cleanup completed successfully!"
#!/usr/bin/env bash
# record-heist-auto.sh — Automated recording of the Heist Movie Night screencast.
# Uses tmux send-keys to drive the TUI interaction while asciinema records.
#
# Prerequisites:
# - Demo project set up at /tmp/wg-hero-demo (run setup-demo.sh first)
# - wg service running in the demo dir (wg service start)
# - tmux, asciinema installed

set -euo pipefail

DEMO_DIR="${WG_DEMO_DIR:-/tmp/wg-hero-demo}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CAST_DIR="$SCRIPT_DIR/recordings"
TMUX_SESSION="wg-heist-rec"
TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
CAST_FILE="$CAST_DIR/heist-${TIMESTAMP}.cast"

PROMPT="Plan a heist movie night for the team — snacks, movie picks, and a debate."

# Validate demo project
if [ ! -d "$DEMO_DIR/.workgraph" ]; then
    echo "ERROR: Demo project not found at $DEMO_DIR. Run setup-demo.sh first."
    exit 1
fi

# Create recordings dir
mkdir -p "$CAST_DIR"

# Kill any existing tmux session with this name
tmux kill-session -t "$TMUX_SESSION" 2>/dev/null || true

echo "=== Automated Heist Movie Night Recording ==="
echo "Output: $CAST_FILE"
echo ""

# Start tmux session with asciinema recording the TUI
tmux new-session -d -s "$TMUX_SESSION" -x 120 -y 36 \
    "cd '$DEMO_DIR' && asciinema rec --idle-time-limit 2 --cols 120 --rows 36 --command 'wg tui' '$CAST_FILE'; echo 'RECORDING_DONE'"

# Force tmux window size
tmux resize-window -t "$TMUX_SESSION" -x 120 -y 36 2>/dev/null || true

echo "Recording started. Waiting for TUI to load..."
sleep 5

# Enter chat input mode (press 'c' in Normal mode)
echo "Entering chat input mode..."
tmux send-keys -t "$TMUX_SESSION" 'c'
sleep 1

# Type the prompt character by character for realistic feel
echo "Typing prompt: $PROMPT"
for (( i=0; i<${#PROMPT}; i++ )); do
    char="${PROMPT:$i:1}"
    tmux send-keys -t "$TMUX_SESSION" -l "$char"
    # Slight delay between characters (30-80ms range for realistic typing)
    sleep 0.0$(( RANDOM % 5 + 3 ))
done

sleep 1

# Submit the prompt (Enter key)
echo "Submitting prompt..."
tmux send-keys -t "$TMUX_SESSION" Enter

echo "Prompt submitted. Waiting for tasks to complete..."

# Count user tasks (non-dot-prefixed) with a given status
count_user_tasks() {
    local status="$1"
    local count
    count=$(cd "$DEMO_DIR" && wg list --status "$status" 2>/dev/null | grep -c '^\[.\] [^.]') || true
    echo "${count:-0}"
}

# Monitor task completion
TIMEOUT=600  # 7 minutes max
ELAPSED=0
POLL_INTERVAL=5

while [ $ELAPSED -lt $TIMEOUT ]; do
    sleep $POLL_INTERVAL
    ELAPSED=$((ELAPSED + POLL_INTERVAL))

    OPEN=$(count_user_tasks "open")
    IN_PROGRESS=$(count_user_tasks "in-progress")
    DONE=$(count_user_tasks "done")
    ACTIVE=$((OPEN + IN_PROGRESS))
    TOTAL=$((ACTIVE + DONE))

    echo "  ${ELAPSED}s: ${DONE}/${TOTAL} done, ${ACTIVE} active (${OPEN} open, ${IN_PROGRESS} in-progress)"

    # If we have user tasks and none are active, we're done
    if [ "$TOTAL" -gt 0 ] && [ "$ACTIVE" -eq 0 ]; then
        echo "All tasks completed!"
        break
    fi

    # Check if tmux session is still alive
    if ! tmux has-session -t "$TMUX_SESSION" 2>/dev/null; then
        echo "ERROR: tmux session died unexpectedly"
        exit 1
    fi
done

if [ $ELAPSED -ge $TIMEOUT ]; then
    echo "WARNING: Timed out waiting for tasks to complete"
fi

# Let the final state render for a moment
sleep 3

# Exit the TUI cleanly: Escape (ensure Normal mode) then q (quit)
echo "Exiting TUI..."
tmux send-keys -t "$TMUX_SESSION" Escape
sleep 0.5
tmux send-keys -t "$TMUX_SESSION" 'q'
sleep 2

# Check if recording was saved
if [ -f "$CAST_FILE" ]; then
    # Get duration from last line
    DURATION=$(tail -1 "$CAST_FILE" | python3 -c "
import sys, json
line = sys.stdin.readline().strip()
if line:
    data = json.loads(line)
    print(f'{data[0]:.1f}')
else:
    print('unknown')
" 2>/dev/null || echo "unknown")

    echo ""
    echo "=== Recording Complete ==="
    echo "File: $CAST_FILE"
    echo "Duration: ${DURATION}s (with 2s idle compression)"
    echo "Preview: asciinema play $CAST_FILE"
else
    echo "ERROR: Recording file not found at $CAST_FILE"
    if tmux has-session -t "$TMUX_SESSION" 2>/dev/null; then
        echo "tmux session still alive — recording may not have stopped properly"
        tmux send-keys -t "$TMUX_SESSION" C-c
        sleep 1
        tmux send-keys -t "$TMUX_SESSION" C-d
        sleep 2
    fi
fi

# Clean up tmux session
tmux kill-session -t "$TMUX_SESSION" 2>/dev/null || true

echo "Done."

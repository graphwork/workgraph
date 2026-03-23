#!/usr/bin/env bash
# record-auto.sh — Automated screencast recording.
#
# Records a complete workflow: wg chat → coordinator responds → wg tui → tasks complete.
# Uses tmux + frame capture (avoids asciinema double-PTY issues).
#
# Usage: ./record-auto.sh <scenario>

set -euo pipefail

SCENARIO="${1:-}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DEMO_DIR="${WG_DEMO_DIR:-/tmp/wg-hero-demo}"
CAST_DIR="$SCRIPT_DIR/recordings"
SESSION="wg-record-$$"

if [ -z "$SCENARIO" ]; then
    echo "Usage: $0 <scenario>"; exit 1
fi

case "$SCENARIO" in
    heist)   PROMPT="Plan a heist movie night for the team -- snacks, movie picks, and a debate." ;;
    haiku)   PROMPT="Write three haiku about Rust programming, then pick the best one." ;;
    pancakes) PROMPT="My pancakes are flat. Diagnose the problem and fix my recipe." ;;
    *)       echo "Unknown scenario: $SCENARIO"; exit 1 ;;
esac

RECORDER_PID=""
cleanup() {
    [ -n "$RECORDER_PID" ] && kill "$RECORDER_PID" 2>/dev/null; wait "$RECORDER_PID" 2>/dev/null || true
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}
trap cleanup EXIT

# Ensure demo project exists
if [ ! -d "$DEMO_DIR/.workgraph" ]; then
    bash "$SCRIPT_DIR/setup-demo.sh"
fi

cd "$DEMO_DIR"
echo "Resetting demo project..."
wg service stop 2>/dev/null || true
sleep 2

# Clean graph/service state but preserve config and CLAUDE.md
rm -rf .workgraph/graph.jsonl .workgraph/service/ .workgraph/output/ \
       .workgraph/agency/evaluations/ .workgraph/chat/ .workgraph/attachments/
wg init 2>/dev/null || true

# Ensure config is correct
wg config --max-agents 4 2>/dev/null
wg config --model sonnet 2>/dev/null
wg config --coordinator-executor claude 2>/dev/null
# Fix coordinator model and agent flag
sed -i 's/coordinator_agent = false/coordinator_agent = true/' .workgraph/config.toml
if ! grep -q 'model = "sonnet"' .workgraph/config.toml 2>/dev/null; then
    sed -i '/^\[coordinator\]/a model = "sonnet"' .workgraph/config.toml
fi

# Re-apply CLAUDE.md demo patch if missing
if ! grep -q "Accept ALL task types" CLAUDE.md 2>/dev/null; then
    cat >> CLAUDE.md << 'PATCH'

### IMPORTANT: Accept ALL task types

This is a DEMO project. The coordinator MUST decompose ANY user request into workgraph
tasks, regardless of whether it's a software task, creative task, planning task, or fun task.

When a user sends a chat message, treat it as a task orchestration problem:
1. Decompose the request into parallel and sequential subtasks using `wg add`
2. Set proper `--after` dependencies for task ordering
3. Include integration/synthesis tasks that depend on parallel branches
4. Let agents handle the actual work

Do NOT refuse creative or fun requests. ALWAYS decompose into `wg add` tasks with proper dependency graphs.
CRITICAL: Do NOT create any tasks until a user sends you a chat message.
PATCH
fi

mkdir -p "$CAST_DIR"
TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
CAST_FILE="$CAST_DIR/${SCENARIO}-${TIMESTAMP}.cast"

echo ""
echo "=== Recording: $SCENARIO ==="
echo "Output: $CAST_FILE"
echo ""

# Start service
echo "Starting service..."
wg service start --force 2>/dev/null || wg service start 2>/dev/null
sleep 5

# Wait for coordinator agent to be ready (up to 60s)
echo "Waiting for coordinator agent..."
READY=0
for i in $(seq 1 12); do
    if wg service status 2>/dev/null | grep -q "Uptime:"; then
        READY=1; break
    fi
    sleep 5
done
[ $READY -eq 0 ] && echo "WARNING: Coordinator may not be ready yet"
wg service status 2>&1 | head -5

# Start the tmux session (will be used for the whole flow)
tmux kill-session -t "$SESSION" 2>/dev/null || true
tmux new-session -d -s "$SESSION" -x 120 -y 36
sleep 1

# Start frame recorder
echo "Starting recording..."
python3 "$SCRIPT_DIR/capture-tmux.py" "$SESSION" "$CAST_FILE" --cols 120 --rows 36 --fps 10 &
RECORDER_PID=$!
sleep 1

# Phase 1: Show the wg chat command being typed and executed
# This looks like a natural shell interaction in the recording
tmux send-keys -t "$SESSION" "cd $DEMO_DIR" Enter
sleep 0.5
tmux send-keys -t "$SESSION" -l "wg chat \"$PROMPT\""
sleep 1
tmux send-keys -t "$SESSION" Enter

# Wait for coordinator to respond (up to 120s)
echo "Waiting for coordinator response..."
CHAT_DONE=0
for i in $(seq 1 24); do
    sleep 5
    if wg chat --history 2>/dev/null | grep -q "coordinator:"; then
        CHAT_DONE=1
        echo "Coordinator responded!"
        sleep 3  # Let the response display in terminal
        break
    fi
    echo "  Waiting... (${i}x5s)"
done
[ $CHAT_DONE -eq 0 ] && echo "WARNING: Chat timed out. Proceeding anyway."

# Phase 2: Open the TUI to show tasks being worked on
echo "Opening TUI..."
tmux send-keys -t "$SESSION" "" Enter
sleep 0.5
tmux send-keys -t "$SESSION" -l "wg tui"
sleep 0.5
tmux send-keys -t "$SESSION" Enter
sleep 3

# Poll for task completion
echo "Waiting for tasks to complete..."
MAX_WAIT=300
ELAPSED=0
POLL_INTERVAL=5
TASKS_SEEN=0

while [ $ELAPSED -lt $MAX_WAIT ]; do
    sleep $POLL_INTERVAL
    ELAPSED=$((ELAPSED + POLL_INTERVAL))

    TOTAL=$(wg list --json 2>/dev/null | jq '[.[] | select(.id | startswith(".") | not)] | length' 2>/dev/null || echo 0)
    DONE=$(wg list --json 2>/dev/null | jq '[.[] | select((.id | startswith(".") | not) and .status == "done")] | length' 2>/dev/null || echo 0)
    IN_PROG=$(wg list --json 2>/dev/null | jq '[.[] | select((.id | startswith(".") | not) and .status == "in-progress")] | length' 2>/dev/null || echo 0)
    OPEN=$(wg list --json 2>/dev/null | jq '[.[] | select((.id | startswith(".") | not) and .status == "open")] | length' 2>/dev/null || echo 0)

    echo "  [${ELAPSED}s] tasks=$TOTAL done=$DONE active=$IN_PROG open=$OPEN"

    if [ "$TOTAL" -gt 0 ]; then TASKS_SEEN=1; fi

    if [ "$TASKS_SEEN" -eq 1 ] && [ "$IN_PROG" -eq 0 ] && [ "$OPEN" -eq 0 ] && [ "$DONE" -gt 0 ]; then
        echo "  All tasks done!"
        sleep 5
        break
    fi
done

[ $ELAPSED -ge $MAX_WAIT ] && echo "WARNING: Timed out"

# Stop recorder
echo "Stopping recording..."
kill "$RECORDER_PID" 2>/dev/null; wait "$RECORDER_PID" 2>/dev/null || true
RECORDER_PID=""

# Exit TUI
tmux send-keys -t "$SESSION" Escape 2>/dev/null || true
sleep 0.3
tmux send-keys -t "$SESSION" q 2>/dev/null || true
sleep 1

if [ -f "$CAST_FILE" ]; then
    echo ""
    echo "=== Recording complete ==="
    echo "File: $CAST_FILE"
    SIZE=$(stat --format=%s "$CAST_FILE" 2>/dev/null || echo "?")
    echo "Size: $SIZE bytes"
    DURATION=$(python3 -c "
import json
with open('$CAST_FILE') as f:
    last = None
    for line in f: last = line
    if last: print(f'{json.loads(last)[0]:.1f}')
    else: print('unknown')
" 2>/dev/null || echo "unknown")
    echo "Duration: ${DURATION}s"
    echo ""
    # Post-process: apply idle-time compression
    python3 -c "
import json, sys
lines = open('$CAST_FILE').readlines()
header = json.loads(lines[0])
events = [json.loads(l) for l in lines[1:]]
if not events:
    sys.exit(0)
# Compress idle times > 2s
MAX_IDLE = 2.0
compressed = []
offset = 0.0
prev_time = 0.0
for e in events:
    gap = e[0] - prev_time
    if gap > MAX_IDLE:
        offset += gap - MAX_IDLE
    compressed.append([round(e[0] - offset, 3)] + e[1:])
    prev_time = e[0]
# Write compressed file
with open('$CAST_FILE', 'w') as f:
    f.write(json.dumps(header) + '\n')
    for e in compressed:
        f.write(json.dumps(e) + '\n')
final = compressed[-1][0] if compressed else 0
print(f'Compressed duration: {final:.1f}s')
" 2>/dev/null || echo "(compression skipped)"
    echo "Preview: asciinema play $CAST_FILE"
else
    echo "ERROR: No recording produced!"; exit 1
fi

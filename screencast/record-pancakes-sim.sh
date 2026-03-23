#!/bin/bash
# record-pancakes-sim.sh — Produce an improved pancakes demo recording.
#
# Shows: wg viz (empty) → wg tui with chat history showing user prompt +
# coordinator response → tasks appearing → task progression with parallel
# execution → completion.
#
# Uses capture-tmux.py to record the TUI in tmux.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DEMO_DIR="/tmp/wg-pancakes-demo-$$"
CAST_FILE="$SCRIPT_DIR/recordings/pancakes-sim-raw.cast"
SESSION="wg-pancakes-$$"
FPS=20

cleanup() {
    kill "$RECORDER_PID" 2>/dev/null; wait "$RECORDER_PID" 2>/dev/null || true
    tmux kill-session -t "$SESSION" 2>/dev/null || true
    (cd "$DEMO_DIR" && wg service stop 2>/dev/null) || true
    rm -rf "$DEMO_DIR"
}
trap cleanup EXIT

RECORDER_PID=""

# === Step 1: Set up a fresh demo project ===
echo "Setting up demo project at $DEMO_DIR..."
mkdir -p "$DEMO_DIR"
cd "$DEMO_DIR"
git init -q && git commit --allow-empty -m init -q
wg init 2>/dev/null
wg config --max-agents 4

# Pre-populate chat history (loaded by TUI on startup)
cat > .workgraph/chat-history.json << 'CHATEOF'
[{"role":"user","text":"My pancakes are flat. Diagnose the problem and fix my recipe.","timestamp":"2026-03-23T00:00:01+00:00","edited":false},{"role":"assistant","text":"I'll help diagnose your flat pancakes! Let me break this down into parallel tasks:\n\n1. **diagnose-flat-pancakes** — Investigate common causes\n2. **fix-pancake-recipe** — Apply the fix (after diagnosis)\n3. **improve-presentation** — Make them look great (parallel with fix)\n4. **final-taste-test** — Verify the result\n\nCreating the task graph now...","timestamp":"2026-03-23T00:00:05+00:00","edited":false}]
CHATEOF

# Pre-create the tasks (all open)
wg add "Diagnose flat pancakes" --id diagnose-flat-pancakes \
  -d "Investigate common causes of flat pancakes: leavening agents, over-mixing, griddle temperature"
wg add "Fix pancake recipe" --id fix-pancake-recipe --after diagnose-flat-pancakes \
  -d "Apply recipe fixes based on diagnosis findings"
wg add "Improve presentation" --id improve-presentation --after diagnose-flat-pancakes \
  -d "Stacking technique, garnish selection, plate presentation"
wg add "Final taste test" --id final-taste-test --after fix-pancake-recipe,improve-presentation \
  -d "Verify the pancakes are fluffy, golden, and delicious"

echo "Tasks created:"
wg viz

# === Step 2: Start service (max-agents 0 so it doesn't interfere with simulation) ===
echo ""
echo "Starting service (no agent spawning)..."
wg config --max-agents 0
wg service start --force 2>/dev/null || wg service start 2>/dev/null
sleep 3

# Restore max-agents for display purposes but service won't spawn any
echo "Starting TUI in tmux session '$SESSION'..."
tmux kill-session -t "$SESSION" 2>/dev/null || true
tmux new-session -d -s "$SESSION" -x 120 -y 35 "cd $DEMO_DIR && wg tui"
sleep 3

# === Step 3: Start recording ===
echo "Starting recorder at ${FPS}fps..."
python3 "$SCRIPT_DIR/capture-tmux.py" "$SESSION" "$CAST_FILE" --cols 120 --rows 35 --fps "$FPS" &
RECORDER_PID=$!
sleep 1

# === Step 4: Show initial state (tasks all open, chat visible) ===
echo "Recording initial state..."
sleep 2

# Navigate to a middle task so BOTH upstream (magenta) and downstream (cyan)
# edges are visible. fix-pancake-recipe has upstream (diagnose) and downstream
# (final-taste-test) connections.
echo "Navigating to middle task (fix-pancake-recipe)..."
tmux send-keys -t "$SESSION" Down   # Select first task (diagnose-flat-pancakes)
sleep 0.3
tmux send-keys -t "$SESSION" Down   # Move to fix-pancake-recipe (middle of graph)
sleep 3                              # Hold so viewer sees both edge directions

# === Step 5: Simulate task progression ===
echo "Simulating task progression..."

# diagnose-flat-pancakes → in-progress
wg claim diagnose-flat-pancakes 2>/dev/null
echo "  diagnose-flat-pancakes: in-progress"
sleep 4

# diagnose-flat-pancakes → done
wg done diagnose-flat-pancakes 2>/dev/null
echo "  diagnose-flat-pancakes: done"
sleep 1

# Parallel: fix-pancake-recipe + improve-presentation → in-progress
wg claim fix-pancake-recipe 2>/dev/null
wg claim improve-presentation 2>/dev/null
echo "  fix-pancake-recipe + improve-presentation: in-progress (PARALLEL)"
sleep 5

# improve-presentation → done (finishes first)
wg done improve-presentation 2>/dev/null
echo "  improve-presentation: done"
sleep 2

# fix-pancake-recipe → done
wg done fix-pancake-recipe 2>/dev/null
echo "  fix-pancake-recipe: done"
sleep 1

# final-taste-test → in-progress
wg claim final-taste-test 2>/dev/null
echo "  final-taste-test: in-progress"
sleep 4

# final-taste-test → done
wg done final-taste-test 2>/dev/null
echo "  final-taste-test: done (ALL COMPLETE)"
sleep 3

# === Step 6: Stop recording ===
echo ""
echo "Stopping recorder..."
kill "$RECORDER_PID" 2>/dev/null; wait "$RECORDER_PID" 2>/dev/null || true
RECORDER_PID=""

# Exit TUI
tmux send-keys -t "$SESSION" q 2>/dev/null || true
sleep 1
tmux kill-session -t "$SESSION" 2>/dev/null || true

if [ -f "$CAST_FILE" ]; then
    LINES=$(wc -l < "$CAST_FILE")
    DURATION=$(python3 -c "
import json
with open('$CAST_FILE') as f:
    lines = f.readlines()
    if len(lines) > 1:
        last = json.loads(lines[-1])
        print(f'{last[0]:.1f}')
    else:
        print('0')
" 2>/dev/null || echo "unknown")
    echo ""
    echo "=== Recording complete ==="
    echo "File: $CAST_FILE"
    echo "Lines: $LINES"
    echo "Duration: ${DURATION}s"
else
    echo "ERROR: No recording produced!"
    exit 1
fi

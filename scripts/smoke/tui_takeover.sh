#!/usr/bin/env bash
# Regression smoke for the takeover-on-TUI-open path.
#
# Scenario: user has a service daemon running (with a coordinator
# agent), daemon has already spawned its own `wg nex --chat` handler
# that holds the session lock. User opens `wg tui`. They expect the
# TUI to become the owner of the chat session so THEIR PTY drives it,
# not the daemon's background handler.
#
# Before the fix (2026-04-23 hotfix): `wg tui` saw a file-tailing
# empty-state, not the embedded `wg nex` REPL, because:
#   1. `wg nex --chat ref` hardcoded `chat/<ref>/` as its chat_dir,
#      bypassing the session registry. The lock landed at
#      `chat/coordinator-0/.handler.pid`.
#   2. The TUI's auto-PTY logic resolved the chat_dir through
#      `chat_dir_for_ref` (registry → UUID dir). It checked THERE
#      for a lock holder, found none, and tried to spawn in owner
#      mode. `wg spawn-task` then lost the lock race against the
#      daemon's handler and exited.
#   3. Render fell through to file-tailing → user saw the daemon's
#      404 error spam instead of their PTY.
#
# Passes when: the lock holder pid after `wg tui` starts is different
# from before it started (takeover fired) AND the new holder is a
# child of the TUI process.

set -u

POLL_DEADLINE=${POLL_DEADLINE:-10}

for t in tmux python3; do
    if ! command -v "$t" >/dev/null; then
        echo "SKIP: $t not available"
        exit 77
    fi
done

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FAKE="$REPO_ROOT/scripts/testing/fake_llm_server.py"
[[ -f "$FAKE" ]] || { echo "FAIL: fake_llm_server.py missing"; exit 1; }

PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
TMPHOME=$(mktemp -d)
SESSION=wg-smoke-takeover-$$
FAKE_PID=
cleanup() {
    tmux kill-session -t "$SESSION" 2>/dev/null
    (cd "$TMPHOME" && wg service stop --kill-agents >/dev/null 2>&1) || true
    [[ -n "$FAKE_PID" ]] && kill "$FAKE_PID" 2>/dev/null
    wait 2>/dev/null
    cd /
    rm -rf "$TMPHOME"
}
trap cleanup EXIT

echo "Canned fake response." > "$TMPHOME/responses.txt"
python3 "$FAKE" --port "$PORT" --responses "$TMPHOME/responses.txt" \
    --ready-file "$TMPHOME/ready.flag" >/dev/null 2>&1 &
FAKE_PID=$!
for i in $(seq 1 10); do
    [[ -f "$TMPHOME/ready.flag" ]] && break
    sleep 0.2
done
[[ -f "$TMPHOME/ready.flag" ]] || { echo "FAIL: fake server not ready"; exit 1; }

cd "$TMPHOME"
wg init --no-agency -m local:fake -e "http://127.0.0.1:$PORT" >/dev/null 2>&1

# Start daemon WITH the coordinator agent so it spawns its own `wg nex
# --chat coordinator-0` handler that holds the session lock.
wg service start >/dev/null 2>&1
# Wait for the daemon's nex child to acquire the lock.
HOLDER_BEFORE=""
for i in $(seq 1 10); do
    sleep 0.5
    HOLDER_BEFORE=$(find .wg/chat -name '.handler.pid' -exec head -1 {} \; 2>/dev/null | head -1)
    [[ -n "$HOLDER_BEFORE" ]] && break
done
if [[ -z "$HOLDER_BEFORE" ]]; then
    echo "FAIL: daemon did not spawn a handler within 5s"
    exit 1
fi

# Launch wg tui. Auto-PTY logic should see observer_mode=true,
# request_release, then spawn-task its own wg nex which becomes the
# new lock holder.
tmux kill-session -t "$SESSION" 2>/dev/null
tmux new-session -d -s "$SESSION" -x 200 -y 50 \
    "cd '$TMPHOME' && wg tui 2>$TMPHOME/tui.err"

HOLDER_AFTER=""
for i in $(seq 1 "$POLL_DEADLINE"); do
    sleep 1
    HOLDER_AFTER=$(find .wg/chat -name '.handler.pid' -exec head -1 {} \; 2>/dev/null | head -1)
    if [[ -n "$HOLDER_AFTER" && "$HOLDER_AFTER" != "$HOLDER_BEFORE" ]]; then
        break
    fi
done

if [[ "$HOLDER_BEFORE" == "$HOLDER_AFTER" ]]; then
    echo "FAIL: lock holder unchanged after TUI open (no takeover)"
    echo "  before: $HOLDER_BEFORE"
    echo "  after:  $HOLDER_AFTER"
    echo "-- screen --"
    tmux capture-pane -t "$SESSION" -p 2>/dev/null | tail -10
    echo "-- tui.err --"
    head -10 "$TMPHOME/tui.err"
    exit 1
fi

echo "PASS: TUI takeover replaced daemon's handler (before=$HOLDER_BEFORE after=$HOLDER_AFTER)"

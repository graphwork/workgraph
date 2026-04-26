#!/usr/bin/env bash
# End-to-end UX smoke: `wg tui` Chat tab drives a real PTY-embedded
# `wg nex` against a fake oai-compat LLM server, types two turns of
# dialogue, and asserts the canned responses render in the Chat pane.
#
# Tests the full UX stack: auto-PTY toggle → wg nex REPL rendering →
# key forwarding → HTTP request → SSE response → TUI render pipeline.
#
# Runs in CI. Exits 0 on pass, 1 on fail with diagnostics, 77 if a
# prerequisite (tmux / python3) is missing (automake "skipped" code).

set -u

POLL_DEADLINE=${POLL_DEADLINE:-10}

# Prereq check — skip (not fail) when the env can't run the test.
need_tools=(tmux python3)
for t in "${need_tools[@]}"; do
    if ! command -v "$t" >/dev/null; then
        echo "SKIP: $t not available"
        exit 77
    fi
done

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FAKE_SERVER="$REPO_ROOT/scripts/testing/fake_llm_server.py"
if [[ ! -f "$FAKE_SERVER" ]]; then
    echo "FAIL: fake_llm_server.py missing at $FAKE_SERVER"
    exit 1
fi

# Random high port to reduce collision with local dev.
PORT=$(python3 -c 'import socket,sys; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')

TMPHOME=$(mktemp -d)
SESSION=wg-smoke-chat-turn-$$
READY="$TMPHOME/fake.ready"
FAKE_PID=
cleanup() {
    tmux kill-session -t "$SESSION" 2>/dev/null
    [[ -n "$FAKE_PID" ]] && kill "$FAKE_PID" 2>/dev/null
    wait 2>/dev/null
    cd /
    rm -rf "$TMPHOME"
}
trap cleanup EXIT

# Canned two-turn script: the fake serves these in order.
cat > "$TMPHOME/responses.txt" <<'EOF'
Hello traveler! What brings you here?

Glad to help — what else would you like to know?
EOF

# Start fake server, wait for it to signal ready.
python3 "$FAKE_SERVER" \
    --port "$PORT" \
    --responses "$TMPHOME/responses.txt" \
    --ready-file "$READY" \
    >"$TMPHOME/fake.stdout" 2>"$TMPHOME/fake.stderr" &
FAKE_PID=$!

for i in $(seq 1 20); do
    [[ -f "$READY" ]] && break
    sleep 0.2
done
if [[ ! -f "$READY" ]]; then
    echo "FAIL: fake server did not become ready"
    cat "$TMPHOME/fake.stderr"
    exit 1
fi

# Init a fresh workgraph pointing at the fake endpoint.
cd "$TMPHOME"
wg init --no-agency -x nex -m local:fake-model -e "http://127.0.0.1:$PORT" >/dev/null 2>&1

# Register a coordinator-1 session alias + the graph task so auto-PTY
# has something to spawn into.
python3 - <<PY
import json, pathlib
wg = pathlib.Path.cwd() / ".wg"
sess = wg / "chat" / "sessions.json"
sess.parent.mkdir(parents=True, exist_ok=True)
uuid = "019db700-0000-7000-8000-000000000042"
sess.write_text(json.dumps({
    "version": 0,
    "sessions": {uuid: {
        "kind": "coordinator",
        "created": "2026-04-22T21:00:00Z",
        "aliases": ["coordinator-1", "1"],
        "label": "test",
    }}
}))
(wg / "chat" / uuid).mkdir(parents=True, exist_ok=True)
PY
wg add ".coordinator-1" --id .coordinator-1 --tag coordinator-loop >/dev/null 2>&1

# NO daemon started: auto-PTY mode writes directly to inbox.jsonl
# (see `send_chat_message` in viz_viewer/state.rs). The PTY-spawned
# `wg nex --chat` tails inbox, hits the fake server, writes outbox —
# TUI's polling cycle picks up outbox. Full round-trip with zero IPC.

# Launch wg tui in a detached tmux session.
tmux kill-session -t "$SESSION" 2>/dev/null
tmux new-session -d -s "$SESSION" -x 180 -y 40 \
    "cd '$TMPHOME' && wg tui 2>$TMPHOME/tui.err"

# ---- Assertion 1: wg nex banner appears in the Chat pane ----
wait_for() {
    local needle="$1"
    for i in $(seq 1 "$POLL_DEADLINE"); do
        sleep 1
        if tmux capture-pane -t "$SESSION" -p 2>/dev/null | grep -qF -- "$needle"; then
            return 0
        fi
    done
    return 1
}

if ! wait_for "wg nex — interactive session"; then
    echo "FAIL: wg nex did not appear in Chat pane"
    echo "-- screen --"
    tmux capture-pane -t "$SESSION" -p 2>/dev/null | head -30
    echo "-- tui.err --"
    head -40 "$TMPHOME/tui.err"
    exit 1
fi

# Key sequence per turn (right-panel focused after auto-PTY,
# chat_pty_forwards_stdin=true so keys go straight to the wg nex
# interactive REPL — no TUI composer involvement):
#   1. Type the message. Each char goes through vendor_pty_active
#      branch in handle_right_panel_key → pane.send_key.
#   2. Enter → submits to nex's rustyline input → nex hits fake
#      server → response streams back through stdout → rendered
#      inside the PTY pane via vt100.

# ---- Turn 1: send "hi there", expect canned response 1 ----
tmux send-keys -t "$SESSION" "hi there"
sleep 0.3
tmux send-keys -t "$SESSION" Enter

if ! wait_for "Hello traveler"; then
    echo "FAIL: turn 1 response not rendered"
    echo "-- screen --"
    tmux capture-pane -t "$SESSION" -p 2>/dev/null | head -30
    echo "-- fake.stderr --"
    cat "$TMPHOME/fake.stderr"
    echo "-- fake.stdout --"
    cat "$TMPHOME/fake.stdout"
    exit 1
fi

# ---- Turn 2: follow-up, expect canned response 2 ----
tmux send-keys -t "$SESSION" "tell me more"
sleep 0.3
tmux send-keys -t "$SESSION" Enter

if ! wait_for "Glad to help"; then
    echo "FAIL: turn 2 response not rendered"
    echo "-- screen --"
    tmux capture-pane -t "$SESSION" -p 2>/dev/null | head -30
    exit 1
fi

# ---- Final sanity: live wg nex child still present ----
if ! pgrep -f "wg nex.*--role coordinator" >/dev/null; then
    echo "FAIL: wg nex child missing after two turns"
    exit 1
fi

echo "PASS: two-turn dialogue round-tripped through TUI + PTY + fake LLM"

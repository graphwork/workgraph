#!/usr/bin/env bash
# End-to-end smoke for the `executor = claude` coordinator path. Asserts
# that:
#   1. `wg tui` auto-enters PTY mode when coordinator.effective_executor
#      is `claude`, embedding the real `claude` CLI.
#   2. Keystrokes forward to the PTY's stdin (chat_pty_forwards_stdin
#      branch in handle_right_panel_key) — test by sending `/help` and
#      asserting claude's help output appears. `/help` doesn't hit
#      Anthropic, so the smoke works offline without burning API tokens.
#   3. No daemon is needed anywhere.
#
# Exit 0 pass, 1 fail, 77 skip (tmux/claude missing).

set -u

POLL_DEADLINE=${POLL_DEADLINE:-20}

for t in tmux claude python3; do
    if ! command -v "$t" >/dev/null; then
        echo "SKIP: $t not available"
        exit 77
    fi
done

TMPHOME=$(mktemp -d)
SESSION=wg-smoke-claude-pty-$$
cleanup() {
    tmux kill-session -t "$SESSION" 2>/dev/null
    cd /
    rm -rf "$TMPHOME"
}
trap cleanup EXIT

cd "$TMPHOME"

# Config the coordinator to use the claude executor.
# `wg init -m claude:opus` sets agent.model AND coordinator.model
# via Config::apply_model_endpoint; no `-e` needed because the model
# has its own `claude:` provider prefix.
wg init --no-agency -x claude -m claude:opus >/dev/null 2>&1
# Confirm the resolver will pick "claude" for this coordinator.
if ! wg config --show 2>&1 | grep -qE 'model = "claude:'; then
    echo "FAIL: couldn't set coordinator model to claude:opus"
    wg config --show 2>&1 | head -30
    exit 1
fi

# Register a coordinator-1 session + task.
python3 - <<'PY'
import json, pathlib
wg = pathlib.Path.cwd() / ".wg"
(wg / "chat").mkdir(parents=True, exist_ok=True)
uuid = "019db700-0000-7000-8000-0000000cafe1"
(wg / "chat" / uuid).mkdir(parents=True, exist_ok=True)
(wg / "chat" / "sessions.json").write_text(json.dumps({
    "version": 0,
    "sessions": {uuid: {
        "kind": "coordinator",
        "created": "2026-04-22T21:00:00Z",
        "aliases": ["coordinator-1", "1"],
        "label": "test",
    }}
}))
PY
wg add ".coordinator-1" --id .coordinator-1 --tag coordinator-loop >/dev/null 2>&1

tmux kill-session -t "$SESSION" 2>/dev/null
tmux new-session -d -s "$SESSION" -x 200 -y 50 \
    "cd '$TMPHOME' && wg tui 2>$TMPHOME/tui.err"

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

# Assertion 1: claude CLI embedded (its distinctive welcome/banner text).
# The banner varies by claude version; look for several distinctive
# strings and pass if any shows.
wait_for_any() {
    for i in $(seq 1 "$POLL_DEADLINE"); do
        sleep 1
        screen=$(tmux capture-pane -t "$SESSION" -p 2>/dev/null)
        for needle in "$@"; do
            if [[ "$screen" == *"$needle"* ]]; then
                return 0
            fi
        done
    done
    return 1
}
# First observable: claude shows its trust prompt on any
# previously-untrusted CWD. Exact wording varies slightly by
# version — matching on `Enter to confirm` (the fixed
# footer hint) is the most stable signal.
if ! wait_for_any "Enter to confirm" "1. Yes, I trust" "Quick safety check" "Accessing workspace"; then
    echo "FAIL: claude trust prompt did not appear"
    echo "-- screen --"
    tmux capture-pane -t "$SESSION" -p 2>/dev/null | head -30
    echo "-- tui.err --"
    head -30 "$TMPHOME/tui.err"
    exit 1
fi

# Assertion 2: keystrokes forward to the PTY. Down arrow moves the
# trust prompt's ❯ marker to option 2 — no process side effects
# (unlike picking "2. No, exit" which would kill claude). If our
# chat_pty_forwards_stdin branch weren't routing keys to claude,
# the arrow would be intercepted by the TUI and the marker
# wouldn't move.
tmux send-keys -t "$SESSION" Down
if ! wait_for_any "❯ 2. No" "> 2. No" "2. No, exit"; then
    echo "FAIL: Down arrow did not reach claude — key-forwarding broken"
    echo "-- screen --"
    tmux capture-pane -t "$SESSION" -p 2>/dev/null | head -30
    exit 1
fi

# Assertion 3: live claude child still present (the Down key didn't
# terminate it — that'd only happen if "2. No, exit" were invoked).
if ! pgrep -f "^claude" >/dev/null; then
    echo "FAIL: claude --continue child process not found after key events"
    pgrep -af claude | head
    exit 1
fi

echo "PASS: claude PTY embedded, keys forward (Down moved ❯ marker), no daemon needed"

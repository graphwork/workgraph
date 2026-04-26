#!/usr/bin/env bash
# Escape-from-PTY smoke. The auto-PTY mode captures every keystroke
# (that's the whole point — claude/nex/codex need raw stdin). But the
# user must be able to get back to the graph. Two escape mechanisms:
#
#   1. Keyboard: Ctrl-T toggles PTY mode off. After that, keys route
#      to the TUI normally.
#   2. Focus: if focused_panel is Graph, vendor_pty_active gates to
#      false. A mouse click on the graph naturally shifts focus;
#      this smoke simulates it by asserting Ctrl-T does NOT get
#      forwarded (would be 0x14 = DC4 on the wire) — it's consumed
#      by the PTY-toggle handler.
#
# Exit 0 pass, 1 fail, 77 skip.

set -u

POLL_DEADLINE=${POLL_DEADLINE:-15}

for t in tmux python3; do
    if ! command -v "$t" >/dev/null; then
        echo "SKIP: $t not available"
        exit 77
    fi
done

TMPHOME=$(mktemp -d)
SESSION=wg-smoke-escape-$$
DUMP_PREFIX="$TMPHOME/pty_dump"
cleanup() {
    tmux kill-session -t "$SESSION" 2>/dev/null
    pkill -f "wg nex .*--role coordinator" 2>/dev/null
    cd /
    rm -rf "$TMPHOME"
}
trap cleanup EXIT

cd "$TMPHOME"

wg init --no-agency -x nex -m local:m -e http://127.0.0.1:1 >/dev/null 2>&1
python3 - <<'PY'
import json, pathlib
wg = pathlib.Path.cwd() / ".wg"
(wg / "chat").mkdir(parents=True, exist_ok=True)
uuid = "019db700-0000-7000-8000-0000000000ef"
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
    "cd '$TMPHOME' && WG_PTY_DUMP='$DUMP_PREFIX' wg tui 2>$TMPHOME/tui.err"

# Wait for the PTY to be up (input dump file appears on first keystroke,
# but spawn creates the file eagerly — just wait for any `.in.bin`).
DUMP_FILE=""
for i in $(seq 1 "$POLL_DEADLINE"); do
    sleep 1
    DUMP_FILE=$(ls "$DUMP_PREFIX".*.in.bin 2>/dev/null | head -1)
    [[ -n "$DUMP_FILE" ]] && break
done
if [[ -z "$DUMP_FILE" ]]; then
    echo "FAIL: PTY didn't spawn"
    head -20 "$TMPHOME/tui.err"
    exit 1
fi

# Baseline: send a plain 'x' to establish that keys ARE reaching PTY
# right now (otherwise we can't distinguish "escape worked" from
# "keys broken").
tmux send-keys -t "$SESSION" "x"
sleep 0.5
BASELINE=$(stat -c %s "$DUMP_FILE")
if [[ "$BASELINE" -lt 1 ]]; then
    echo "FAIL: initial keystroke didn't reach PTY input stream"
    exit 1
fi

# Press Ctrl-T. If this forwards to PTY, we'd see 0x14 appended.
# If the escape handler consumes it, the input stream stays at baseline.
tmux send-keys -t "$SESSION" "C-t"
sleep 0.7

POST_TOGGLE=$(stat -c %s "$DUMP_FILE")
if [[ "$POST_TOGGLE" -gt "$BASELINE" ]]; then
    # Byte was forwarded — Ctrl-T escape is broken.
    echo "FAIL: Ctrl-T appears to have been forwarded to the PTY ($POST_TOGGLE > $BASELINE)"
    echo "-- tail of input dump (hex) --"
    tail -c 16 "$DUMP_FILE" | xxd
    exit 1
fi

# After the toggle, sending another key should now go to the TUI, not
# the PTY. Check: send '0' (which, in TUI Normal mode, switches to tab
# 0 = Chat — already there, so it's idempotent — but more importantly
# shouldn't land in the PTY input stream).
tmux send-keys -t "$SESSION" "0"
sleep 0.5
POST_ZERO=$(stat -c %s "$DUMP_FILE")
if [[ "$POST_ZERO" -gt "$POST_TOGGLE" ]]; then
    echo "FAIL: key AFTER Ctrl-T toggle still forwarded to PTY"
    echo "   baseline=$BASELINE post-toggle=$POST_TOGGLE post-zero=$POST_ZERO"
    echo "-- tail of input dump (hex) --"
    tail -c 16 "$DUMP_FILE" | xxd
    exit 1
fi

echo "PASS: Ctrl-T toggled PTY mode off (baseline=$BASELINE ≡ post=$POST_ZERO)"

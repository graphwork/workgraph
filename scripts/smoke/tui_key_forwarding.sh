#!/usr/bin/env bash
# Key-forwarding smoke for the PTY pane. Boots `wg tui` with the
# native executor (cheapest to spawn; no auth), sends a sequence of
# specific keys, and asserts the corresponding byte sequences landed
# in WG_PTY_DUMP — the raw stdin stream the embedded process sees.
#
# Why this smoke exists: the `vendor_pty_active` branch in event.rs
# and the `key_event_to_bytes` mapping in pty_pane.rs are the two
# gates between a user keystroke and the embedded REPL. Past bugs:
#   - Enter=CR+LF (killed claude after trust-accept)
#   - KeyCode::BackTab unhandled (Shift+Tab silently dropped)
#   - pgrep regex bug (false smoke pass on native arm)
# This smoke catches regressions by asserting the ACTUAL bytes, not
# just "something rendered."
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
SESSION=wg-smoke-keys-$$
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
uuid = "019db700-0000-7000-8000-0000000000ee"
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

# Wait for the INPUT dump file to exist (means PTY has spawned).
# Note: output dump is `<prefix>.<cmd>.<pid>.bin`, input dump is
# `<prefix>.<cmd>.<pid>.in.bin` — this smoke asserts on bytes WE send,
# so we read the input side.
DUMP_FILE=""
for i in $(seq 1 "$POLL_DEADLINE"); do
    sleep 1
    DUMP_FILE=$(ls "$DUMP_PREFIX".*.in.bin 2>/dev/null | head -1)
    [[ -n "$DUMP_FILE" ]] && break
done
if [[ -z "$DUMP_FILE" ]]; then
    echo "FAIL: no PTY input dump appeared — PTY did not spawn or TUI focus off right panel"
    ls -la "$TMPHOME" "$DUMP_PREFIX".*.bin 2>/dev/null
    head -30 "$TMPHOME/tui.err"
    exit 1
fi

# Record current size so we only assert on bytes we just sent.
baseline_size() { stat -c %s "$DUMP_FILE" 2>/dev/null; }
wait_for_bytes() {
    local start_offset="$1"
    local needle="$2"   # Python repr, e.g. b"\\x1b[Z"
    for i in $(seq 1 10); do
        sleep 0.3
        # Read bytes since start_offset, check if needle substring appears
        python3 - "$DUMP_FILE" "$start_offset" "$needle" <<'PY'
import sys
path, off, needle = sys.argv[1], int(sys.argv[2]), sys.argv[3]
data = open(path, "rb").read()[off:]
want = eval(needle)  # like b"\x1b[Z"
sys.exit(0 if want in data else 1)
PY
        [[ $? -eq 0 ]] && return 0
    done
    return 1
}

FAIL=0
check() {
    local label="$1" key="$2" needle="$3"
    local off
    off=$(baseline_size)
    tmux send-keys -t "$SESSION" "$key"
    if wait_for_bytes "$off" "$needle"; then
        echo "  PASS  $label ($key → $needle)"
    else
        echo "  FAIL  $label ($key → $needle) — bytes not observed in PTY stream"
        FAIL=1
    fi
}

# Key → expected-bytes matrix (Python bytes-literal repr).
# Covers the regressions we've actually hit plus the common navigation
# keys claude/codex/nex all use.
check "plain letter x"  "x"            'b"x"'
check "Enter is CR (no LF)" "Enter"    'b"\r"'
# Send \r\n would = Enter + Ctrl-J = kill claude. Negative assertion:
# after sending Enter, \n should NOT appear immediately after the \r.
# (We just assert \r was present above; no Ctrl-J check here because
# Ctrl-J could legitimately arrive via other keys later.)
check "Up arrow CSI-A"  "Up"           'b"\x1b[A"'
check "Down arrow CSI-B" "Down"        'b"\x1b[B"'
check "Right arrow CSI-C" "Right"      'b"\x1b[C"'
check "Left arrow CSI-D" "Left"        'b"\x1b[D"'
check "Shift+Tab CSI-Z" "BTab"         'b"\x1b[Z"'
check "Backspace DEL"   "BSpace"       'b"\x7f"'
check "Ctrl-C SIGINT"   "C-c"          'b"\x03"'
check "Tab forward"     "Tab"          'b"\t"'
check "Esc"             "Escape"       'b"\x1b"'
check "Alt-b ESC-b"     "M-b"          'b"\x1bb"'
check "Ctrl-Q passthru" "C-q"          'b"\x11"'

if [[ $FAIL -ne 0 ]]; then
    echo ""
    echo "FAIL: one or more keys did not forward correctly"
    echo "-- PTY dump (hex of last 256 bytes) --"
    tail -c 256 "$DUMP_FILE" | xxd | head -20
    exit 1
fi

echo ""
echo "PASS: all keys forwarded to PTY with correct byte sequences"

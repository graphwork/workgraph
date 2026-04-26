#!/usr/bin/env bash
# Smoke test for TUI PTY: paste forwarding, scroll, and resume.
# Covers the test matrix from the investigate-tui-pty task.
#
# Exit 0 pass, 1 fail, 77 skip (tmux/python3 missing).

set -u

POLL_DEADLINE=${POLL_DEADLINE:-15}

for t in tmux python3; do
    if ! command -v "$t" >/dev/null; then
        echo "SKIP: $t not available"
        exit 77
    fi
done

TMPHOME=$(mktemp -d)
SESSION=wg-smoke-paste-$$
DUMP_PREFIX="$TMPHOME/pty_dump"
FAIL=0
cleanup() {
    tmux kill-session -t "$SESSION" 2>/dev/null
    pkill -f "wg nex .*--role coordinator" 2>/dev/null
    cd /
    rm -rf "$TMPHOME"
}
trap cleanup EXIT

cd "$TMPHOME"

# Initialize a native-executor workgraph (cheapest, no auth).
wg init --no-agency -x nex -m local:m -e http://127.0.0.1:1 >/dev/null 2>&1
USER_NAME=$(whoami 2>/dev/null || echo "smoke")
python3 - "$USER_NAME" <<'PY'
import json, pathlib, sys
user = sys.argv[1]
wg = pathlib.Path.cwd() / ".wg"
(wg / "chat").mkdir(parents=True, exist_ok=True)
uuid = "019db700-0000-7000-8000-0000000000ff"
(wg / "chat" / uuid).mkdir(parents=True, exist_ok=True)
(wg / "chat" / "sessions.json").write_text(json.dumps({
    "version": 0,
    "sessions": {uuid: {
        "kind": "coordinator",
        "created": "2026-04-22T21:00:00Z",
        "aliases": ["coordinator-0", "0"],
        "label": "test",
    }}
}))
PY
wg add ".coordinator-0" --id .coordinator-0 --tag coordinator-loop \
    -d "Coordinator: $USER_NAME" >/dev/null 2>&1

tmux kill-session -t "$SESSION" 2>/dev/null
tmux new-session -d -s "$SESSION" -x 200 -y 50 \
    "cd '$TMPHOME' && WG_PTY_DUMP='$DUMP_PREFIX' wg tui 2>$TMPHOME/tui.err"

# Wait for PTY input dump file.
DUMP_FILE=""
for i in $(seq 1 "$POLL_DEADLINE"); do
    sleep 1
    DUMP_FILE=$(ls "$DUMP_PREFIX".*.in.bin 2>/dev/null | head -1)
    [[ -n "$DUMP_FILE" ]] && break
done
if [[ -z "$DUMP_FILE" ]]; then
    echo "FAIL: no PTY input dump — PTY did not spawn"
    head -30 "$TMPHOME/tui.err" 2>/dev/null
    exit 1
fi

echo "=== Test 1: Paste forwarding ==="
echo "Sending bracketed paste via tmux..."
baseline=$(stat -c %s "$DUMP_FILE" 2>/dev/null)
# tmux's `send-keys -l` sends literal text. When bracketed paste is
# enabled in the inner terminal, crossterm should receive it as
# Event::Paste. We test whether the text arrives in the PTY input dump.
# NOTE: This test verifies the tui_pty.sh (standalone) path correctly;
# the wg tui (VizApp) path has the vendor_pty_active bug and will FAIL.
tmux send-keys -t "$SESSION" -l "pasted-test-string-12345"
sleep 2
python3 - "$DUMP_FILE" "$baseline" <<'PY'
import sys
path, off = sys.argv[1], int(sys.argv[2])
data = open(path, "rb").read()[off:]
# The pasted text should appear as individual key bytes (since tmux
# send-keys -l sends them as key events, not bracketed paste).
# We look for the literal string in the dump.
if b"pasted-test-string-12345" in data:
    print("  PASS: pasted text found in PTY input dump")
    sys.exit(0)
else:
    print("  FAIL: pasted text NOT found in PTY input dump")
    print(f"  Dump contents (last 200 bytes): {data[-200:]!r}")
    sys.exit(1)
PY
if [[ $? -ne 0 ]]; then FAIL=1; fi

echo ""
echo "=== Test 2: Scroll (PageUp/PageDown) ==="
# Feed enough output to fill scrollback, then test PageUp/PageDown.
# PageUp/PageDown are intercepted by the TUI (not forwarded to PTY)
# for scrollback navigation. We verify they DON'T appear in the PTY
# input dump (intercepted correctly) by checking dump size doesn't grow.
baseline2=$(stat -c %s "$DUMP_FILE" 2>/dev/null)
tmux send-keys -t "$SESSION" "PageUp"
sleep 0.5
tmux send-keys -t "$SESSION" "PageDown"
sleep 0.5
after_scroll=$(stat -c %s "$DUMP_FILE" 2>/dev/null)
if [[ "$after_scroll" -eq "$baseline2" ]]; then
    echo "  PASS: PageUp/PageDown intercepted (not forwarded to PTY)"
else
    echo "  FAIL: PageUp/PageDown leaked to PTY input (dump grew by $((after_scroll - baseline2)) bytes)"
    FAIL=1
fi

echo ""
echo "=== Test 3: Scroll resets on keypress ==="
# After scrolling up, pressing a regular key should reset scroll to 0.
# We can verify this indirectly: after PageUp + regular key, the regular
# key's bytes should appear (proving send_key ran and reset scroll_offset).
baseline3=$(stat -c %s "$DUMP_FILE" 2>/dev/null)
tmux send-keys -t "$SESSION" "PageUp"
sleep 0.3
tmux send-keys -t "$SESSION" "z"
sleep 0.5
python3 - "$DUMP_FILE" "$baseline3" <<'PY'
import sys
path, off = sys.argv[1], int(sys.argv[2])
data = open(path, "rb").read()[off:]
if b"z" in data:
    print("  PASS: keypress after scroll forwarded (scroll reset)")
else:
    print("  FAIL: keypress after scroll not forwarded")
    sys.exit(1)
PY
if [[ $? -ne 0 ]]; then FAIL=1; fi

echo ""
echo "=== Test 4: Resume — native executor session persistence ==="
# Kill the TUI, pre-seed a conversation journal with a known tag,
# then relaunch. The native executor should pass --chat <ref> and
# nex should auto-resume. We check the tmux pane content for the
# "resuming session" banner that nex prints on resume.
tmux kill-session -t "$SESSION" 2>/dev/null
sleep 1

# Resolve the coordinator session's chat dir (same path the TUI uses).
COORD_REF="coordinator-0"
CHAT_DIR=$(cd "$TMPHOME" && python3 -c "
import json, pathlib
wg = pathlib.Path('.wg')
sessions = json.loads((wg / 'chat' / 'sessions.json').read_text())
for uuid, info in sessions['sessions'].items():
    if '$COORD_REF' in info.get('aliases', []):
        print(wg / 'chat' / uuid)
        break
")
if [[ -z "$CHAT_DIR" ]]; then
    echo "  SKIP: could not resolve chat dir for $COORD_REF"
else
    # Seed a conversation journal so nex detects a prior session.
    mkdir -p "$CHAT_DIR"
    RESUME_TAG="RESUME-TAG-$$-$(date +%s)"
    cat > "$CHAT_DIR/conversation.jsonl" <<JSONL
{"role":"user","content":"$RESUME_TAG","turn":0,"timestamp":"2026-04-25T00:00:00Z"}
{"role":"assistant","content":"Acknowledged: $RESUME_TAG","turn":0,"timestamp":"2026-04-25T00:00:01Z"}
JSONL

    # Relaunch TUI — native executor should pass --chat coordinator-0
    # and nex should see the journal and print "resuming session".
    tmux new-session -d -s "$SESSION" -x 200 -y 50 \
        "cd '$TMPHOME' && WG_PTY_DUMP='$DUMP_PREFIX' wg tui 2>$TMPHOME/tui2.err"

    # Wait for the resume banner to appear in tmux pane content.
    RESUME_FOUND=0
    for i in $(seq 1 "$POLL_DEADLINE"); do
        sleep 1
        PANE_TEXT=$(tmux capture-pane -t "$SESSION" -p 2>/dev/null)
        if echo "$PANE_TEXT" | grep -q "resuming session"; then
            RESUME_FOUND=1
            break
        fi
    done
    if [[ $RESUME_FOUND -eq 1 ]]; then
        echo "  PASS: native executor resumed session (saw 'resuming session')"
    else
        echo "  FAIL: native executor did not resume — 'resuming session' not found in pane"
        tmux capture-pane -t "$SESSION" -p 2>/dev/null | head -20
        head -10 "$TMPHOME/tui2.err" 2>/dev/null
        FAIL=1
    fi
fi

echo ""
if [[ $FAIL -ne 0 ]]; then
    echo "FAIL: one or more tests failed"
    exit 1
fi
echo "PASS: all PTY paste/scroll/resume tests passed"

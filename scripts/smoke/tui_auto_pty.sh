#!/usr/bin/env bash
# Live smoke: `wg tui` must auto-embed `wg nex` in the Chat tab for a
# native-executor coordinator. Reproduces the c76179f53 bug fix
# (task_id-vs-chat_ref split) end-to-end.
#
# Why tmux + headless TUI: the auto-PTY path can only be verified by
# actually running the ratatui event loop, because the bug was in
# live process spawning + filesystem lookup, not in pure logic. Unit
# tests won't catch it.
#
# Passes when: within `$POLL_DEADLINE` seconds the Chat pane contains
# `wg nex — interactive session with` (wg nex's banner) AND there's
# a live `wg nex --chat coordinator-1` child in the process tree.
#
# Run: `bash scripts/smoke/tui_auto_pty.sh` — exits 0 on pass, 1 on
# fail with diagnostics.

set -u

POLL_DEADLINE=${POLL_DEADLINE:-15}
TMPHOME=$(mktemp -d)
SESSION=wg-smoke-auto-pty-$$
trap 'tmux kill-session -t "$SESSION" 2>/dev/null; cd /; rm -rf "$TMPHOME"' EXIT

cd "$TMPHOME"

# 1. Fresh init with native executor (triggers auto-PTY for Chat tab).
wg init --no-agency -x nex -m local:test-model -e http://127.0.0.1:1 >/dev/null 2>&1

# 2. Register a coordinator-1 session so the alias resolves through
#    the registry (same shape `wg tui` creates on its own; baking it
#    here keeps the test deterministic).
python3 - <<'PY'
import json, pathlib
wg = pathlib.Path.cwd() / ".wg"
sess = wg / "chat" / "sessions.json"
sess.parent.mkdir(parents=True, exist_ok=True)
uuid = "019db700-0000-7000-8000-000000000001"
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

# 3. Task in the graph so spawn-task can resolve .coordinator-1.
wg add ".coordinator-1" --id .coordinator-1 --tag coordinator-loop >/dev/null 2>&1

# 4. Launch `wg tui` in a detached tmux session so we can capture the
#    pane without an actual terminal.
tmux kill-session -t "$SESSION" 2>/dev/null
tmux new-session -d -s "$SESSION" -x 180 -y 40 \
    "cd '$TMPHOME' && wg tui 2>$TMPHOME/tui.err; echo DONE; sleep 30"

# 5. Poll the captured screen for the nex banner. Short timeout so CI
#    failures surface fast.
PASS=0
for i in $(seq 1 "$POLL_DEADLINE"); do
    sleep 1
    screen=$(tmux capture-pane -t "$SESSION" -p 2>/dev/null)
    if [[ "$screen" == *"wg nex — interactive session with"* ]]; then
        PASS=1
        break
    fi
done

# 6. Cross-check: a live `wg nex ... --role coordinator` child for THIS
#    test's coordinator-1 must exist. Use --resume coordinator-1 as the
#    needle to avoid matching stale processes from other tests.
NEX_CMDLINE=$(pgrep -af "wg nex.*--resume coordinator-1" 2>/dev/null | head -1)
if [[ -z "$NEX_CMDLINE" ]]; then
    # Fallback: check for any wg nex --role coordinator
    if pgrep -af "wg nex.*--role coordinator" >/dev/null; then
        NEX_CMDLINE=$(pgrep -af "wg nex.*--role coordinator" 2>/dev/null | head -1)
        echo "FAIL: found wg nex but NOT with --resume coordinator-1"
        echo "  cmdline: $NEX_CMDLINE"
    fi
    PASS=0
fi

# 7. Post treat-wg-nex (58eb7c751): nex must use --resume (stdin via
#    rustyline), NOT --chat (inbox.jsonl). If --chat is present, the
#    PTY forwards keystrokes to a stdin nobody reads.
if [[ -n "$NEX_CMDLINE" ]] && echo "$NEX_CMDLINE" | grep -q -- "--chat"; then
    echo "FAIL: nex spawned with --chat (expected --resume after treat-wg-nex)"
    echo "  cmdline: $NEX_CMDLINE"
    PASS=0
fi

if [[ "$PASS" == 1 ]]; then
    echo "PASS: wg nex embedded in Chat tab, live child present, uses --resume"
    exit 0
fi

echo "FAIL: Chat pane did not embed wg nex within ${POLL_DEADLINE}s"
echo "-- tui screen --"
tmux capture-pane -t "$SESSION" -p 2>/dev/null | head -30
echo "-- tui.err --"
head -40 "$TMPHOME/tui.err" 2>/dev/null
echo "-- processes --"
pgrep -af "wg (tui|nex|spawn-task|session attach)" 2>/dev/null
exit 1

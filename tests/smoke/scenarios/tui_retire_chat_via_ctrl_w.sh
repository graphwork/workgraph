#!/usr/bin/env bash
# Scenario: tui_retire_chat_via_ctrl_w
#
# Pins the regression behind `tui-cannot-retire`:
#
#   "man why can't we retire the .chat-4 ???" — user could not get rid
#   of a chat tab in the TUI because the embedded vendor CLI (claude
#   --resume) shows its own session-resume modal that swallows Esc/digit
#   keys, leaving the user stuck unable to reach the wg
#   Archive/Stop/Abandon dialog.
#
# Fix: Ctrl+W is a global escape hatch that breaks out of PTY forwarding
# AND opens the wg retire dialog for the active chat tab. Selecting
# Abandon ('x' hotkey) issues `service delete-coordinator <cid>` which
# marks the chat task abandoned, removing it from the tab bar.
#
# This scenario drives a real `wg tui` inside tmux with three live chats,
# sends Ctrl+W → 'x' → Enter, and asserts the active-chats count goes
# from 3 to 2 (i.e., one chat actually retired). If a future refactor
# breaks Ctrl+W or the choice dialog, this fires.
#
# No LLM is required — uses the shell executor.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

if ! command -v tmux >/dev/null 2>&1; then
    loud_skip "MISSING TMUX" "tmux not on PATH; cannot drive a PTY for the TUI"
fi

scratch=$(make_scratch)
session="wgsmoke-retire-$$"
kill_tmux_session() {
    tmux kill-session -t "$session" 2>/dev/null || true
}
add_cleanup_hook kill_tmux_session
cd "$scratch"

if ! wg init --executor shell >init.log 2>&1; then
    loud_fail "wg init --executor shell failed: $(tail -5 init.log)"
fi

start_wg_daemon "$scratch" --max-agents 1
graph_dir="$WG_SMOKE_DAEMON_DIR"

# Three chats: at least two must remain after one retire so the count
# delta (3 → 2) is unambiguously caused by Ctrl+W rather than a hidden
# default-chat suppression.
for name in alpha beta gamma; do
    out=$(wg chat create --name "$name" 2>&1)
    rc=$?
    if [[ "$rc" -ne 0 ]]; then
        loud_fail "wg chat create --name $name exited with rc=${rc}: ${out}"
    fi
done

tmux new-session -d -s "$session" -x 200 -y 60 "wg tui"

for _ in $(seq 1 30); do
    if [[ -S "$graph_dir/service/tui.sock" ]]; then
        break
    fi
    sleep 0.5
done
if [[ ! -S "$graph_dir/service/tui.sock" ]]; then
    loud_fail "wg tui did not create tui.sock within 15s"
fi

count_visible_chats() {
    wg --json tui-dump 2>/dev/null \
        | grep -oE 'coord:[0-9]+' | sort -u | wc -l
}

# Wait for the tab bar to render all three chats.
initial=0
for _ in $(seq 1 30); do
    initial=$(count_visible_chats)
    if [[ "$initial" -ge 3 ]]; then
        break
    fi
    sleep 0.5
done
if [[ "$initial" -lt 3 ]]; then
    loud_fail "expected ≥3 chat tabs visible before retire, got ${initial} — chats not surfacing in tab bar"
fi
echo "before retire: ${initial} chat tabs visible"

# Move focus to the right panel and ensure we're on the Chat tab — the
# Ctrl+W binding only fires when right_panel_tab == Chat. The TUI
# typically lands on Chat by default, but we send '0' (Chat tab index)
# to be explicit. Ctrl+T puts focus inside the PTY (claude/shell), which
# is the worst-case scenario the fix is meant to cover.
tmux send-keys -t "$session" "0"
sleep 0.3
tmux send-keys -t "$session" "C-t"
sleep 0.5

# Ctrl+W: should break out of PTY forwarding AND open the
# Archive/Stop/Abandon dialog for the active chat.
tmux send-keys -t "$session" "C-w"
sleep 0.5

# Press 'x' (Abandon hotkey) — selects and executes Abandon, which
# issues `service delete-coordinator <cid>`. The dialog closes
# automatically.
tmux send-keys -t "$session" "x"
sleep 1.5

# Allow the daemon a moment to actually mark the chat task abandoned and
# the TUI a moment to refresh its list.
for _ in $(seq 1 20); do
    after=$(count_visible_chats)
    if [[ "$after" -lt "$initial" ]]; then
        break
    fi
    sleep 0.5
done

after=$(count_visible_chats)
echo "after retire: ${after} chat tabs visible"

if [[ "$after" -ge "$initial" ]]; then
    loud_fail "Ctrl+W → 'x' did NOT retire a chat tab (count stayed at ${after}). Either the hotkey is unbound, the dialog didn't open, or the Abandon action didn't reach the dispatcher."
fi

# Sanity: the live tab bar must reflect the deletion (not just an
# in-memory abandonment that the next refresh would reverse).
abandoned_count=0
graph_path="$graph_dir/graph.jsonl"
if [[ -f "$graph_path" ]]; then
    abandoned_count=$(grep -c '"status":"abandoned"' "$graph_path" || true)
fi
if [[ "$abandoned_count" -lt 1 ]]; then
    loud_fail "graph.jsonl shows zero abandoned tasks after Ctrl+W → 'x' — Abandon never reached the graph (delete-coordinator IPC may have failed silently)"
fi

echo "PASS: Ctrl+W → 'x' retired a chat tab (${initial} → ${after}); graph shows ${abandoned_count} abandoned task(s)"
exit 0

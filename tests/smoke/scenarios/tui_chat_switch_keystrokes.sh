#!/usr/bin/env bash
# Scenario: tui_chat_switch_keystrokes
#
# Pins the regression the user filed against `tui-still-cannot`:
#
#   "we still can't actually select a different coordinator chat! the blue
#    highlight is cool for showing us which one we're on. but we have no way
#    to pick! lol. so we made a bunch of .chat-4 and so on but these are not
#    available to us!"
#
# `tui-chat-tab` (commit 5376ed69b) supposedly addressed this — its spec
# said "Number keys (1-9) jump to chat tab N", but nothing fired in the
# live TUI because the handlers were gated to `focused_panel == RightPanel`
# AND the plain-digit shortcut clashed with right-panel tab navigation.
#
# This scenario drives a real `wg tui` inside tmux, reads the active chat
# via the `wg tui-dump` IPC, sends synthetic '2' / '3' / '1' keystrokes,
# and asserts the active chat changes. If a future refactor breaks the
# handler again, this fires.
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
session="wgsmoke-$$"
kill_tmux_session() {
    tmux kill-session -t "$session" 2>/dev/null || true
}
add_cleanup_hook kill_tmux_session
cd "$scratch"

if ! wg init --executor shell >init.log 2>&1; then
    loud_fail "wg init --executor shell failed: $(tail -5 init.log)"
fi

# Start the dispatcher so `wg tui` has live state to display.
start_wg_daemon "$scratch" --max-agents 1
graph_dir="$WG_SMOKE_DAEMON_DIR"

# Create 3 chats via `wg chat create` — this is the path the user actually
# hits, and the tags/IDs it produces (`chat-loop` tag, `.chat-N` IDs) are
# what `list_coordinator_ids_and_labels` must filter on. Earlier versions
# of that filter only matched the legacy `coordinator-loop` tag, which
# silently hid every new chat from the tab bar — half of the
# tui-still-cannot bug.
for name in a b c; do
    out=$(wg chat create --name "$name" 2>&1)
    rc=$?
    if [[ "$rc" -ne 0 ]]; then
        loud_fail "wg chat create --name $name exited with rc=${rc}: ${out}"
    fi
done

# Start `wg tui` inside a detached tmux session so we can send synthetic
# keystrokes via `tmux send-keys`. Use a generous size so the renderer
# has room to draw the coordinator tab bar.
tmux new-session -d -s "$session" -x 200 -y 60 "wg tui"

# Wait for the TUI to come up and write its dump socket.
for _ in $(seq 1 30); do
    if [[ -S "$graph_dir/service/tui.sock" ]]; then
        break
    fi
    sleep 0.5
done
if [[ ! -S "$graph_dir/service/tui.sock" ]]; then
    loud_fail "wg tui did not create tui.sock within 15s"
fi

# Helper: read the active coordinator_id from `wg --json tui-dump`.
get_active_cid() {
    wg --json tui-dump 2>/dev/null \
        | sed -n 's/.*"coordinator_id"[[:space:]]*:[[:space:]]*\([0-9]\+\).*/\1/p' \
        | head -1
}

# Sanity: the tab bar must show ≥3 chats, otherwise the user has no
# multi-tab choice in the first place and the rest of this scenario
# can't tell whether key switching works.
dump_text=$(wg --json tui-dump 2>/dev/null || true)
# Tab labels are the canonical chat task id (`.chat-N`), per task tui-tab-bar.
# The legacy `coord:N` shorthand was removed because it duplicated the
# deprecated `coordinator` role-noun and used a stale 1-indexed counter
# that didn't track the actual task id.
distinct_chat_labels=$(printf '%s' "$dump_text" \
    | grep -oE '\.chat-[0-9]+' | sort -u | wc -l)
if [[ "$distinct_chat_labels" -lt 3 ]]; then
    loud_fail "tab bar only shows ${distinct_chat_labels} .chat-N labels (expected ≥3) — chats are hidden from the tab bar (the list_coordinator_ids_and_labels filter is missing the chat-loop tag, or chats aren't being created with the right tag)"
fi
# Hard-fail if any deprecated `coord:N` label leaked back into the tab bar.
if printf '%s' "$dump_text" | grep -qE 'coord:[0-9]+'; then
    leaked=$(printf '%s' "$dump_text" | grep -oE 'coord:[0-9]+' | sort -u | tr '\n' ' ')
    loud_fail "tab bar still emits deprecated coord:N labels (found: ${leaked}) — labels must use the .chat-N task-id form"
fi

# Wait for the first dump to populate.
initial_cid=""
for _ in $(seq 1 30); do
    initial_cid=$(get_active_cid)
    if [[ -n "$initial_cid" ]]; then
        break
    fi
    sleep 0.5
done
if [[ -z "$initial_cid" ]]; then
    loud_fail "wg tui-dump never returned a coordinator_id"
fi

# The coordinator IDs the user can switch to are 0, 1, and 2 (positional
# tab indices 0, 1, 2 → user keys '1', '2', '3'). We assert each press
# moves the active chat to a *different* cid than before (the handler
# uses positional indexing, so the exact mapping depends on graph order).

# Helper: send a single key and wait until the dumped cid differs from the
# previous one (or the wait times out).
press_and_wait_change() {
    local key="$1"
    local prev="$2"
    tmux send-keys -t "$session" "$key"
    for _ in $(seq 1 20); do
        local cur
        cur=$(get_active_cid)
        if [[ -n "$cur" && "$cur" != "$prev" ]]; then
            echo "$cur"
            return 0
        fi
        sleep 0.25
    done
    echo ""
    return 1
}

# Press 'X' (a no-op) just to wake up the renderer / settle initial state.
sleep 0.5

# Start by snapshotting the current cid.
cid_a=$(get_active_cid)
echo "initial active coordinator_id=$cid_a"

# Press '2' → should move to a different chat.
cid_b=$(press_and_wait_change "2" "$cid_a") || \
    loud_fail "pressing '2' did NOT change active chat (still cid=${cid_a}). The number-key chat-switch handler is broken — exactly the tui-still-cannot regression."
echo "after '2': active coordinator_id=$cid_b"

# Press '3' → should move to yet another chat.
cid_c=$(press_and_wait_change "3" "$cid_b") || \
    loud_fail "pressing '3' did NOT change active chat (still cid=${cid_b})"
echo "after '3': active coordinator_id=$cid_c"

# Press '1' → should return to the first chat (likely cid_a, but at minimum != cid_c).
cid_d=$(press_and_wait_change "1" "$cid_c") || \
    loud_fail "pressing '1' did NOT change active chat (still cid=${cid_c})"
echo "after '1': active coordinator_id=$cid_d"

# Sanity: at least 2 distinct cids visited (would catch a handler that
# always switches to cid 0 regardless of key).
distinct=$(printf '%s\n%s\n%s\n%s\n' "$cid_a" "$cid_b" "$cid_c" "$cid_d" | sort -u | wc -l)
if [[ "$distinct" -lt 3 ]]; then
    loud_fail "only ${distinct} distinct cids visited across '2'/'3'/'1' presses (cids: $cid_a $cid_b $cid_c $cid_d) — handler is not actually selecting different chats"
fi

echo "PASS: number-key chat-switch produced ${distinct} distinct chats: $cid_a → $cid_b → $cid_c → $cid_d"
exit 0

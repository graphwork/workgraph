#!/usr/bin/env bash
# Scenario: tui_log_pane_renders_raw_stream
#
# Regression: the Log pane (right-panel tab '4') showed
# "(no agent output yet — is the task running?)" even when an in-progress
# task's assigned agent had a populated raw_stream.jsonl. The original
# tui-agent-activity work added the parsing + rendering machinery but the
# render-time lazy-load path forgot to call update_log_stream_events()
# alongside load_log_pane() and update_log_output(). Result: stream events
# only refreshed on the slow 1s tick / fs-change debounce, NOT on the first
# draw of the Log tab. To the user, the tab looked permanently broken.
#
# This scenario:
#   1. Boots a synthetic .wg layout with one in-progress task assigned to
#      a fake agent whose raw_stream.jsonl already contains JSONL events.
#   2. Launches `wg tui` inside tmux, lets it draw, sends '4' to switch
#      to the Log tab.
#   3. Uses `wg tui-dump` to read the rendered cell grid back out.
#   4. Asserts the dump (a) does NOT contain the broken-state sentinel
#      and (b) DOES contain a unique marker we placed in the stream file.
#
# Requires: tmux, python3, wg on PATH.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

if ! command -v tmux >/dev/null 2>&1; then
    loud_skip "MISSING TMUX" "tmux not on PATH; cannot drive interactive TUI"
fi
if ! command -v python3 >/dev/null 2>&1; then
    loud_skip "MISSING PYTHON3" "python3 needed to mutate graph.jsonl"
fi

scratch=$(make_scratch)
session="wgsmoke-tuilog-$$"
kill_tmux_session() {
    tmux kill-session -t "$session" 2>/dev/null || true
}
add_cleanup_hook kill_tmux_session

cd "$scratch"

if ! wg init -x claude >init.log 2>&1; then
    loud_fail "wg init failed during smoke setup: $(tail -5 init.log)"
fi

graph_dir=""
for cand in .wg .workgraph; do
    if [[ -f "$scratch/$cand/graph.jsonl" ]]; then
        graph_dir="$scratch/$cand"
        break
    fi
done
if [[ -z "$graph_dir" ]]; then
    loud_fail "could not locate graph.jsonl under .wg/ or .workgraph/ after init"
fi

if ! wg add "Live agent task" --id smoke-live >add.log 2>&1; then
    loud_fail "wg add failed during smoke setup: $(tail -5 add.log)"
fi

# Mark the task in-progress and assigned to agent-fake.
python3 - "$graph_dir/graph.jsonl" <<'PY'
import json, sys
path = sys.argv[1]
out = []
for line in open(path):
    if not line.strip():
        continue
    obj = json.loads(line)
    if obj.get("kind") == "task" and obj.get("id") == "smoke-live":
        obj["status"] = "in-progress"
        obj["assigned"] = "agent-fake"
    out.append(json.dumps(obj))
open(path, "w").write("\n".join(out) + "\n")
PY

# Place a raw_stream.jsonl that mimics the format claude-handler writes.
mkdir -p "$graph_dir/agents/agent-fake"
marker="WG_TUI_LOG_SMOKE_MARKER_$$"
cat >"$graph_dir/agents/agent-fake/raw_stream.jsonl" <<EOF
{"type":"system","subtype":"init","cwd":"$scratch","session_id":"smoke","tools":["Bash"]}
{"type":"assistant","message":{"content":[{"type":"text","text":"$marker"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"echo from-smoke"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"from-smoke","is_error":false}]}}
EOF
: >"$graph_dir/agents/agent-fake/output.log"

# Launch wg tui in tmux. Wide window so the Log pane has room.
tmux new-session -d -s "$session" -x 200 -y 60 "cd $scratch && wg tui"
sleep 4

# Esc out of the chat PTY focus that the chat tab grabs by default,
# then '4' switches the right panel to Log.
tmux send-keys -t "$session" 'Escape'
sleep 1
tmux send-keys -t "$session" '4'
sleep 3

# Pull the rendered screen back out via the dump server.
dump_out="$scratch/dump.txt"
if ! ( cd "$scratch" && wg tui-dump >"$dump_out" 2>&1 ); then
    loud_fail "wg tui-dump failed:\n$(cat "$dump_out")"
fi

if grep -q "no agent output yet" "$dump_out"; then
    loud_fail "Log pane still shows 'no agent output yet' despite raw_stream.jsonl having events.\nDump:\n$(cat "$dump_out")"
fi

if ! grep -q "$marker" "$dump_out"; then
    loud_fail "Log pane did not render the unique stream marker '$marker'.\nDump:\n$(cat "$dump_out")"
fi

# Auto-refresh check: append a new event and verify it shows up on a
# subsequent dump (within a few ticks).
marker2="WG_TUI_LOG_SMOKE_NEW_$$"
printf '\n{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}]}}\n' "$marker2" \
    >>"$graph_dir/agents/agent-fake/raw_stream.jsonl"
sleep 3

dump_out2="$scratch/dump2.txt"
( cd "$scratch" && wg tui-dump >"$dump_out2" 2>&1 ) || true

if ! grep -q "$marker2" "$dump_out2"; then
    loud_fail "Log pane did not pick up newly-appended stream event '$marker2' after 3s.\nDump:\n$(cat "$dump_out2")"
fi

echo "PASS: Log tab renders raw_stream.jsonl events and auto-refreshes"
exit 0

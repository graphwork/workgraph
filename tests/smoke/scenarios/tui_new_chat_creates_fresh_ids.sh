#!/usr/bin/env bash
# Scenario: tui_new_chat_creates_fresh_ids
#
# Pins the regression the user filed against `tui-new-chat`:
#
#   "it doesnt convert into chat-2 or whatever it takes over the last
#    coordinator chat"
#
# The TUI launcher dialog drives `wg service create-coordinator` via IPC.
# Each invocation MUST allocate a fresh `.chat-N` task; the next id is
# `max(existing_ids) + 1`, never a reuse of an existing slot. If a future
# refactor breaks this invariant (e.g. by counting only alive chats and
# wrapping the id), the user's "it overwrote my chat" complaint comes
# back.
#
# We run the IPC three times and assert three distinct task ids in the
# graph. No claude/native LLM call is made — this is a graph + IPC
# correctness test, runnable with no API credentials.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
cd "$scratch"

# Use shell executor so this runs without LLM credentials.
if ! wg init --executor shell >init.log 2>&1; then
    loud_fail "wg init --executor shell failed: $(tail -5 init.log)"
fi

start_wg_daemon "$scratch" --max-agents 1
graph_dir="$WG_SMOKE_DAEMON_DIR"

# Three back-to-back create-coordinator IPCs. Each must succeed and
# return a JSON blob with a distinct task_id.
ids=()
for i in 1 2 3; do
    out=$(wg service create-coordinator --name "smoke-${i}" --executor shell 2>&1)
    rc=$?
    if [[ "$rc" -ne 0 ]]; then
        loud_fail "create-coordinator #${i} exited with rc=${rc}: ${out}"
    fi
    # Extract task_id (line like:  "task_id": ".chat-N",)
    tid=$(printf '%s\n' "$out" | sed -n 's/.*"task_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
    if [[ -z "$tid" ]]; then
        loud_fail "create-coordinator #${i} produced no task_id; output:\n${out}"
    fi
    ids+=("$tid")
done

# Assert all three ids are distinct.
uniq_count=$(printf '%s\n' "${ids[@]}" | sort -u | wc -l)
if [[ "$uniq_count" -ne 3 ]]; then
    loud_fail "expected 3 distinct chat ids, got: ${ids[*]} (uniq=${uniq_count})"
fi

# Assert all three appear in the graph.jsonl.
graph_jsonl="$graph_dir/graph.jsonl"
if [[ ! -f "$graph_jsonl" ]]; then
    loud_fail "graph.jsonl not found at $graph_jsonl"
fi
for tid in "${ids[@]}"; do
    if ! grep -q "\"id\":\"${tid}\"" "$graph_jsonl"; then
        loud_fail "task ${tid} missing from graph.jsonl"
    fi
done

echo "PASS: 3 distinct fresh chat task ids created via IPC: ${ids[*]}"
exit 0

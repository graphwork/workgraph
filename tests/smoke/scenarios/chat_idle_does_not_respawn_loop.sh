#!/usr/bin/env bash
# Scenario: chat_idle_does_not_respawn_loop
#
# chat-agent-loops-2 regression check: an idle chat task (no inbox messages)
# must NOT be re-Opened by cycle reactivation. The original bug had .chat-2
# dispatched 458 times in autohaiku/workgraph because every `wg done` on the
# chat task synchronously re-Opened it via Mode 2 implicit cycle iteration,
# which the supervisor then re-spawned, which called `wg done` again, etc.
#
# This scenario exercises the synchronous reactivation path that the unit
# tests cover (graph::reactivate_cycle), but at the binary level — so a
# future refactor that bypasses the guard via a different code path is
# caught by `wg done` against a real chat-loop tagged task.
#
# Strategy:
#   1. wg init + wg service start --no-chat-agent --max-agents 0  (no LLM)
#   2. wg service create-chat --name idle-test  → creates `.chat-N` with
#      tags=["chat-loop"] and cycle_config (unlimited, no_converge=true)
#   3. Stop the supervisor process for that chat task (so the agent doesn't
#      actually run) — we just want the task in the graph
#   4. Run `wg done .chat-N` once, then read graph.jsonl
#   5. ASSERT: chat task status is "done" (not re-Opened to "open")
#   6. ASSERT: loop_iteration stayed at 0 (not incremented)
#   7. ASSERT: task log has at most ONE "Re-activated by cycle iteration"
#      entry (allow zero — we expect zero, but tolerate a single legacy
#      entry for forward-compat with the `--converged` semantics)
#
# No LLM credentials needed. No model calls. Pure graph-state regression.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
cd "$scratch"

if ! wg init -x shell >init.log 2>&1; then
    if ! wg init -x claude >init.log 2>&1; then
        loud_fail "wg init failed: $(tail -5 init.log)"
    fi
fi

# Boot daemon WITHOUT a chat agent so we don't need credentials. We also
# pin --max-agents 0 so the dispatcher doesn't try to spawn workers on any
# auto-created scaffold tasks while we're poking at the graph.
start_wg_daemon "$scratch" --max-agents 0 --no-chat-agent
graph_dir="$WG_SMOKE_DAEMON_DIR"

# Create a chat task via IPC. The bug repros without ever sending a message.
if ! wg service create-chat --name idle-test >create.log 2>&1; then
    loud_fail "wg service create-chat failed: $(tail -10 create.log)"
fi

# Find the .chat-N id assigned (could be 0 if first chat).
chat_id=""
for n in $(seq 0 9); do
    if grep -qE "\"id\":\"\\.chat-${n}\"" "$graph_dir/graph.jsonl"; then
        chat_id=".chat-${n}"
        break
    fi
done
if [[ -z "$chat_id" ]]; then
    loud_fail "no .chat-N task found in graph.jsonl after create-chat:\n$(tail -20 "$graph_dir/graph.jsonl")"
fi

# Sanity: verify the task has the chat-loop tag — this is the precondition
# for the bug. Without the tag, the guard wouldn't even be exercised.
if ! grep -qE "\"id\":\"${chat_id//./\\.}\".*\"chat-loop\"" "$graph_dir/graph.jsonl"; then
    loud_fail "chat task ${chat_id} is missing the 'chat-loop' tag — bug-class precondition not met:\n$(grep "\"id\":\"${chat_id//./\\.}\"" "$graph_dir/graph.jsonl" | tail -1)"
fi

# Run wg done. This is the synchronous reactivation path. With the bug,
# the task would be re-Opened in microseconds and loop_iteration bumped.
if ! wg done "$chat_id" >done.log 2>&1; then
    # `wg done` may refuse if the task is in_progress with no agent; allow
    # via stop-chat first to put it in a clean done-able state.
    wg service stop-chat "${chat_id##.chat-}" >stop.log 2>&1 || true
    if ! wg done "$chat_id" >done.log 2>&1; then
        loud_fail "wg done $chat_id failed: $(tail -10 done.log)"
    fi
fi

# Sleep briefly to allow any async dispatcher tick to fire too.
sleep 3

# Read the final graph state.
last_chat_line=$(grep -E "\"id\":\"${chat_id//./\\.}\"" "$graph_dir/graph.jsonl" | tail -1)
if [[ -z "$last_chat_line" ]]; then
    loud_fail "chat task ${chat_id} disappeared from graph.jsonl after wg done"
fi

# ASSERT 1: status MUST be "done" (not re-Opened by cycle reactivation).
if ! echo "$last_chat_line" | grep -qE '"status":"done"'; then
    loud_fail "chat task ${chat_id} status is NOT done after wg done — cycle reactivation re-Opened it. Line:\n$last_chat_line"
fi

# ASSERT 2: loop_iteration MUST be 0 (cycle never iterated).
loop_iter=$(echo "$last_chat_line" \
    | grep -oE '"loop_iteration":[0-9]+' \
    | grep -oE '[0-9]+' \
    | head -1)
loop_iter="${loop_iter:-0}"
if [[ "$loop_iter" -gt 0 ]]; then
    loud_fail "chat task ${chat_id} loop_iteration=$loop_iter (expected 0) — cycle iterated against an event-driven task. Line:\n$last_chat_line"
fi

# ASSERT 3: task log MUST contain zero "Re-activated by cycle iteration" entries.
reactivation_count=$(echo "$last_chat_line" \
    | grep -oE 'Re-activated by cycle iteration' \
    | wc -l)
if [[ "$reactivation_count" -gt 0 ]]; then
    loud_fail "chat task ${chat_id} log shows $reactivation_count cycle reactivations (expected 0). Line:\n$last_chat_line"
fi

echo "PASS: chat task ${chat_id} stayed Done after wg done — no cycle reactivation (loop_iteration=$loop_iter, reactivations=$reactivation_count)"
exit 0

#!/usr/bin/env bash
# Scenario: reconciler_clears_open_with_dead_agent
#
# Lazy / safety-net path described in design-claim-lifecycle: a task in
# Status::Open whose `assigned` references an agent that no longer
# exists (Dead, missing, or unreachable PID) must be unclaimed by the
# dispatcher's reconciliation tick — even when no `wg reset` / `wg
# retry` ever runs.
#
# This handles failure modes the eager paths cannot:
#   - kill -9 of the dispatcher itself
#   - panic-on-startup before the agent claims the task properly
#   - host reboot mid-flight
#
# `wg sweep` shares the predicate with the dispatcher reconciler
# (`reconcile_orphaned_tasks`), so we use it as the user-facing proxy
# for the dispatcher tick — same code path, no need to spin up a real
# daemon and wait for poll_interval.
#
# What this scenario asserts:
#   1. Build: an Open task claimed by a never-registered agent.
#   2. Run:   wg sweep
#   3. After: task.assigned == None AND task.status == Open
#   4. After: log records the sweep entry
#   5. After: the task appears in `wg ready`

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
cd "$scratch"

if ! wg init -m claude:opus >init.log 2>&1; then
    loud_fail "wg init failed: $(tail -10 init.log)"
fi

if ! wg add "Reconciler victim" --id ready-task -d "smoke" >>add.log 2>&1; then
    loud_fail "wg add failed: $(tail -5 add.log)"
fi

# Claim with an actor that has no live registry entry. Pre-fix the
# reconciler ignored Open tasks entirely; post-fix it surfaces this as
# orphaned because `agent-zombie-1` is absent from the agent registry.
if ! wg claim ready-task --actor agent-zombie-1 >>claim.log 2>&1; then
    loud_fail "wg claim failed: $(tail -5 claim.log)"
fi

# wg claim flips the task to InProgress. To exercise the
# Status::Open-with-stale-claim branch (the bug-retry scenario shape),
# unset InProgress without unsetting `assigned`. We do that via the
# dedicated mutator:
#   1. wg unclaim (would clear assigned too — wrong)
#   2. wg reset (also clears assigned — wrong, that's the eager path)
#   3. Direct reset of status only — there is no public CLI for that,
#      so instead we use the more realistic shape: skip wg claim and
#      handcraft the row.
#
# Reset to clean state and rebuild via direct edit of graph.jsonl.
if ! wg reset ready-task --yes >>reset.log 2>&1; then
    loud_fail "wg reset failed during fixture build: $(tail -5 reset.log)"
fi

graph_dir=$(graph_dir_in "$scratch") || loud_fail "no .workgraph dir to edit"
graph_path="$graph_dir/graph.jsonl"

# Stamp a stale `assigned` on the (already-Open) task without flipping
# status. This simulates the agency-assigner-stamped-then-upstream-died
# failure mode the bug-retry doc describes.
python3 - <<PY >>fixture.log 2>&1 || loud_fail "python3 fixture build failed: $(tail -5 fixture.log)"
import json, sys
path = "$graph_path"
out_lines = []
patched = False
with open(path) as f:
    for line in f:
        line = line.rstrip("\n")
        if not line.strip():
            out_lines.append(line); continue
        obj = json.loads(line)
        if obj.get("kind") == "task" and obj.get("id") == "ready-task":
            obj["assigned"] = "agent-zombie-1"
            patched = True
        out_lines.append(json.dumps(obj))
if not patched:
    sys.exit("ready-task row not found in graph.jsonl")
with open(path, "w") as f:
    for line in out_lines:
        f.write(line + "\n")
PY

# Sanity: task is Open with stale claim.
out=$(wg show ready-task 2>&1) || loud_fail "wg show ready-task failed:\n$out"
if ! echo "$out" | grep -qE "Status:[[:space:]]*open"; then
    loud_fail "fixture broken — task not Open before sweep:\n$out"
fi
if ! echo "$out" | grep -qE "Assigned:[[:space:]]*agent-zombie-1"; then
    loud_fail "fixture broken — stale claim not stamped before sweep:\n$out"
fi

# Note: `wg ready` lists Open tasks regardless of `assigned`; the
# dispatcher's spawn loop is what actually skips claimed tasks. We
# don't assert pre-state on `wg ready` for that reason; we go straight
# to the post-sweep assertion that the claim was cleared.

# Operation under test: wg sweep should detect & fix the stale claim.
if ! wg sweep >sweep.log 2>&1; then
    loud_fail "wg sweep failed: $(tail -10 sweep.log)"
fi

# Assertion: assigned cleared.
out=$(wg show ready-task 2>&1) || loud_fail "wg show ready-task failed:\n$out"
if echo "$out" | grep -qE "Assigned:[[:space:]]*agent-zombie-1"; then
    loud_fail "stale claim survived wg sweep — reconciler does not handle Status::Open:\n$out"
fi

# Assertion: still Open (the operation must not have killed the task).
if ! echo "$out" | grep -qE "Status:[[:space:]]*open"; then
    loud_fail "task is not Open after sweep:\n$out"
fi

# Assertion: log records the sweep entry.
if ! echo "$out" | grep -qiE "sweep|reconcil"; then
    loud_fail "task log missing sweep/reconcile entry after fix:\n$out"
fi

# Assertion: task appears in `wg ready` now (claim cleared = dispatchable).
ready_post=$(wg ready 2>&1)
if ! echo "$ready_post" | grep -q "ready-task"; then
    loud_fail "task not ready after sweep cleared the stale claim:\n$ready_post"
fi

echo "PASS: lazy reconciler / wg sweep clears Status::Open + stale-claim tasks"
exit 0

#!/usr/bin/env bash
# Scenario: reset_clears_claim
#
# Regression: `wg reset` resets task status to Open but did NOT clear the
# `assigned` field. Result: tasks claimed by dead agents from a previous run
# stayed claimed across reset, so `wg ready` returned nothing and the
# dispatcher refused to spawn — looking identical to the dispatcher-poll bug
# from the user's POV ("I reset and resumed, why is nothing happening?").
#
# After the fix, `wg reset` mirrors `wg unclaim`: it clears `assigned` and
# `started_at` so the task is immediately ready for a fresh dispatcher pickup.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
cd "$scratch"

# Init with a minimal handler so we don't need any external endpoint.
if ! wg init -m claude:opus >init.log 2>&1; then
    loud_fail "wg init failed during smoke setup: $(tail -10 init.log)"
fi

# Create three tasks and claim each with a distinct (fake) actor.
for id in alpha beta gamma; do
    if ! wg add "Task $id" --id "task-$id" -d "smoke" >>add.log 2>&1; then
        loud_fail "wg add failed for task-$id: $(tail -5 add.log)"
    fi
    if ! wg claim "task-$id" --actor "agent-dead-$id" >>claim.log 2>&1; then
        loud_fail "wg claim failed for task-$id: $(tail -5 claim.log)"
    fi
done

# Sanity check: while claimed, none of the tasks should be ready.
if wg ready 2>&1 | grep -qE "task-(alpha|beta|gamma)"; then
    loud_fail "tasks were ready while still claimed — claim semantics broken upstream of reset"
fi

# Reset all three. This is the operation under test.
if ! wg reset task-alpha --seeds task-beta,task-gamma --yes >reset.log 2>&1; then
    loud_fail "wg reset failed: $(tail -10 reset.log)"
fi

# After reset, every task must be Open with no assigned actor.
for id in alpha beta gamma; do
    out=$(wg show "task-$id" 2>&1) || loud_fail "wg show task-$id failed:\n$out"
    if ! echo "$out" | grep -qE "Status:[[:space:]]*open"; then
        loud_fail "task-$id is not Open after reset:\n$out"
    fi
    if echo "$out" | grep -qE "Assigned:[[:space:]]*agent-dead"; then
        loud_fail "task-$id still has stale assigned field after reset:\n$out"
    fi
done

# All three must now appear in `wg ready`.
ready_out=$(wg ready 2>&1)
for id in alpha beta gamma; do
    if ! echo "$ready_out" | grep -q "task-$id"; then
        loud_fail "task-$id not ready after reset (should be):\n$ready_out"
    fi
done

echo "PASS: wg reset clears claim fields and tasks are immediately ready"
exit 0

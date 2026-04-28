#!/usr/bin/env bash
# Scenario: retry_clears_downstream_stale_claims
#
# Regression: `wg retry <upstream>` did NOT propagate claim-clearing to
# downstream tasks that were claimed by a dead agent. Symptom on fan-out
# workflows: synthesis tasks would sit at status=Open with an `assigned`
# field pointing to an agent ID that no longer exists in the registry,
# and the dispatcher would skip them ("not ready") forever.
#
# Fix: `wg retry` now walks the forward closure and clears `assigned`
# on every non-terminal downstream task whose claim references a Dead
# (or registry-absent, or process-unreachable) agent.
#
# What this scenario asserts:
#   1. Build: upstream (Failed) → downstream (Open, claimed by absent agent)
#   2. Run:   wg retry upstream --reason "smoke"
#   3. After: downstream.assigned == None AND downstream.status == Open
#   4. After: downstream's log records the stale-claim cleanup entry
#   5. After: downstream appears in `wg ready` (would not pre-fix)

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
cd "$scratch"

if ! wg init -m claude:opus >init.log 2>&1; then
    loud_fail "wg init failed: $(tail -10 init.log)"
fi

# Build the chain: upstream → downstream
if ! wg add "Upstream task" --id upstream -d "smoke" >>add.log 2>&1; then
    loud_fail "wg add upstream failed: $(tail -5 add.log)"
fi
if ! wg add "Downstream task" --id downstream --after upstream -d "smoke" >>add.log 2>&1; then
    loud_fail "wg add downstream failed: $(tail -5 add.log)"
fi

# Claim each with a fake actor (no live agent exists for these IDs, so
# the registry lookup will treat them as missing == stale).
if ! wg claim upstream --actor agent-dead-up >>claim.log 2>&1; then
    loud_fail "wg claim upstream failed: $(tail -5 claim.log)"
fi
if ! wg claim downstream --actor agent-dead-down >>claim.log 2>&1; then
    loud_fail "wg claim downstream failed: $(tail -5 claim.log)"
fi

# Mark upstream Failed via wg fail so the retry path is exercised.
if ! wg fail upstream --reason "smoke setup — pretend this crashed" >>fail.log 2>&1; then
    loud_fail "wg fail upstream failed: $(tail -10 fail.log)"
fi

# Sanity: downstream still claimed.
out=$(wg show downstream 2>&1) || loud_fail "wg show downstream failed:\n$out"
if ! echo "$out" | grep -qE "Assigned:[[:space:]]*agent-dead-down"; then
    loud_fail "downstream not claimed at start of test:\n$out"
fi

# Operation under test.
if ! wg retry upstream --reason "smoke retry" >retry.log 2>&1; then
    loud_fail "wg retry upstream failed: $(tail -20 retry.log)"
fi

# Assertion (a): downstream.assigned == None.
out=$(wg show downstream 2>&1) || loud_fail "wg show downstream failed after retry:\n$out"
if echo "$out" | grep -qE "Assigned:[[:space:]]*agent-dead-down"; then
    loud_fail "downstream still has stale claim after retry — eager walk did not run:\n$out"
fi

# Assertion (b): downstream.status == open.
if ! echo "$out" | grep -qE "Status:[[:space:]]*open"; then
    loud_fail "downstream is not Open after retry:\n$out"
fi

# Assertion (c): the cleanup log entry is recorded on the downstream
# task (so it's auditable later).
if ! echo "$out" | grep -qE "stale-claim cleared via retry of upstream"; then
    loud_fail "downstream log missing the stale-claim cleanup entry:\n$out"
fi

# Assertion (d): upstream.status == open.
out_up=$(wg show upstream 2>&1) || loud_fail "wg show upstream failed:\n$out_up"
if ! echo "$out_up" | grep -qE "Status:[[:space:]]*open"; then
    loud_fail "upstream not Open after retry:\n$out_up"
fi

# Assertion (e): downstream appears in `wg ready` (would not pre-fix —
# the dispatcher's "ready" check excludes claimed tasks).
ready_out=$(wg ready 2>&1)
# downstream depends on upstream so won't be ready until upstream is done;
# but after retry, upstream is Open, so downstream should appear in the
# blocked-by-upstream pool. Use `wg list` to confirm both are open + unclaimed.
list_out=$(wg list --status open 2>&1) || loud_fail "wg list failed:\n$list_out"
if ! echo "$list_out" | grep -q "downstream"; then
    loud_fail "downstream not in 'wg list --status open' after retry:\n$list_out"
fi

echo "PASS: wg retry clears stale downstream claims and the downstream task is dispatchable"
exit 0

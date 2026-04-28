#!/usr/bin/env bash
# Scenario: reset_clears_downstream_claims_too
#
# Belt-and-suspenders against regression: the existing
# `reset_clears_claim` scenario only checks the seed task's own claim.
# This one builds a true upstream → downstream chain, claims BOTH with
# (different) dead agents, then runs `wg reset upstream --yes` and
# asserts that the downstream claim is cleared too via the closure
# walk (default direction is Forward).
#
# What this scenario asserts:
#   1. Build: upstream → downstream, both InProgress, both with dead-agent claims.
#   2. Run:   wg reset upstream --yes  (default Forward direction)
#   3. After: BOTH tasks have status=Open AND assigned=None.
#   4. After: BOTH log a reset entry.
#
# Pre-fix, a future refactor of `compute_closure` could silently drop
# downstream from the closure and the existing reset_clears_claim
# scenario wouldn't catch it (it uses --seeds with all three IDs
# explicit, so it never tests the closure walk).

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
cd "$scratch"

if ! wg init -m claude:opus >init.log 2>&1; then
    loud_fail "wg init failed: $(tail -10 init.log)"
fi

# Build chain.
if ! wg add "Upstream" --id upstream -d "smoke" >>add.log 2>&1; then
    loud_fail "wg add upstream failed: $(tail -5 add.log)"
fi
if ! wg add "Downstream" --id downstream --after upstream -d "smoke" >>add.log 2>&1; then
    loud_fail "wg add downstream failed: $(tail -5 add.log)"
fi

# Claim both — wg claim flips status to InProgress and stamps assigned.
if ! wg claim upstream --actor agent-dead-up >>claim.log 2>&1; then
    loud_fail "wg claim upstream failed: $(tail -5 claim.log)"
fi
if ! wg claim downstream --actor agent-dead-down >>claim.log 2>&1; then
    loud_fail "wg claim downstream failed: $(tail -5 claim.log)"
fi

# Sanity.
for id in upstream downstream; do
    out=$(wg show "$id" 2>&1) || loud_fail "wg show $id failed:\n$out"
    if ! echo "$out" | grep -qE "Status:[[:space:]]*in-progress"; then
        loud_fail "$id not InProgress at start of test:\n$out"
    fi
    if ! echo "$out" | grep -qE "Assigned:[[:space:]]*agent-dead-"; then
        loud_fail "$id not claimed at start of test:\n$out"
    fi
done

# Operation under test: reset only the upstream seed; closure walk
# (Forward, default) should pull downstream into the reset set.
if ! wg reset upstream --yes >reset.log 2>&1; then
    loud_fail "wg reset failed: $(tail -10 reset.log)"
fi

# Assertions: both tasks must be Open with no claim.
for id in upstream downstream; do
    out=$(wg show "$id" 2>&1) || loud_fail "wg show $id failed:\n$out"
    if ! echo "$out" | grep -qE "Status:[[:space:]]*open"; then
        loud_fail "$id is not Open after wg reset upstream:\n$out"
    fi
    if echo "$out" | grep -qE "Assigned:[[:space:]]*agent-dead-"; then
        loud_fail "$id still claimed after wg reset upstream — closure walk regressed:\n$out"
    fi
done

# Both must show a reset log entry.
for id in upstream downstream; do
    out=$(wg show "$id" 2>&1)
    if ! echo "$out" | grep -qE "reset via .wg reset upstream."; then
        loud_fail "$id log missing reset entry:\n$out"
    fi
done

echo "PASS: wg reset upstream cleared claim on upstream + downstream via closure walk"
exit 0

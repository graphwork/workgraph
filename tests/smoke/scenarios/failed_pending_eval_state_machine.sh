#!/usr/bin/env bash
# Scenario: failed_pending_eval_state_machine
#
# Validates the FailedPendingEval state machine end-to-end without a live LLM:
#
# 1. agent-exit-nonzero + auto_evaluate=true → failed-pending-eval (NOT failed)
# 2. other failure class (api-error-429) + auto_evaluate=true → failed (no rescue)
# 3. wg fail on existing FailedPendingEval → terminal failed (operator override)
# 4. downstream task does NOT become ready when upstream is failed-pending-eval
# 5. .evaluate-X system task DOES become ready (system bypass)
# 6. wg list shows [e] indicator for failed-pending-eval tasks
#
# No daemon, no LLM — pure graph state + wg CLI.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
cd "$scratch"

if ! wg init -x shell >init.log 2>&1; then
    loud_fail "wg init failed: $(tail -5 init.log)"
fi

# wg init -x shell already enables auto_evaluate=true in config.
# Confirm the config exists under .wg/ (canonical name after wg init).
if [[ ! -f .wg/config.toml ]]; then
    loud_fail "expected .wg/config.toml after wg init"
fi

# ── Test 1: agent-exit-nonzero + auto_evaluate → failed-pending-eval ──────────
wg add "task-a" --id task-a >add_a.log 2>&1
wg claim task-a >claim_a.log 2>&1 || true

wg fail task-a --class agent-exit-nonzero --reason "smoke test exit" >fail_a.log 2>&1
status_a=$(wg show task-a --json 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('status','?'))" 2>/dev/null || wg show task-a 2>&1 | grep -i "^status:" | head -1 | awk '{print $2}')

if [[ "$status_a" != "failed-pending-eval" ]]; then
    loud_fail "Test 1 FAIL: expected failed-pending-eval after agent-exit-nonzero with auto_evaluate=true, got: $status_a"
fi
echo "PASS (1/6): agent-exit-nonzero + auto_evaluate=true → failed-pending-eval (got: $status_a)"

# ── Test 2: api-error-429 + auto_evaluate → failed (not rescued) ──────────────
wg add "task-b" --id task-b >add_b.log 2>&1
wg claim task-b >claim_b.log 2>&1 || true

wg fail task-b --class api-error-429-rate-limit --reason "rate limit smoke" >fail_b.log 2>&1
status_b=$(wg show task-b --json 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('status','?'))" 2>/dev/null || wg show task-b 2>&1 | grep -i "^status:" | head -1 | awk '{print $2}')

if [[ "$status_b" != "failed" ]]; then
    loud_fail "Test 2 FAIL: expected failed after api-error-429, got: $status_b"
fi
echo "PASS (2/6): api-error-429 → failed (no rescue path)"

# ── Test 3: operator wg fail on FailedPendingEval → terminal failed ───────────
# task-a is now in failed-pending-eval; call wg fail again
wg fail task-a --class agent-exit-nonzero --reason "operator forced terminal" >force_fail_a.log 2>&1
status_a2=$(wg show task-a --json 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('status','?'))" 2>/dev/null || wg show task-a 2>&1 | grep -i "^status:" | head -1 | awk '{print $2}')

if [[ "$status_a2" != "failed" ]]; then
    loud_fail "Test 3 FAIL: expected terminal failed after operator wg fail on FailedPendingEval, got: $status_a2"
fi
echo "PASS (3/6): operator wg fail on FailedPendingEval → terminal failed"

# ── Test 4: downstream NOT ready when upstream is failed-pending-eval ─────────
wg add "source-c" --id source-c >add_c.log 2>&1
wg claim source-c >claim_c.log 2>&1 || true
wg fail source-c --class agent-exit-nonzero >fail_c.log 2>&1

# Add downstream that depends on source-c
wg add "downstream-c" --id downstream-c --after source-c >add_dc.log 2>&1

status_source=$(wg show source-c --json 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('status','?'))" 2>/dev/null || echo "?")
if [[ "$status_source" != "failed-pending-eval" ]]; then
    loud_fail "Test 4 setup FAIL: source-c should be failed-pending-eval, got: $status_source"
fi

ready_output=$(wg ready 2>&1)
if echo "$ready_output" | grep -q "downstream-c"; then
    loud_fail "Test 4 FAIL: downstream-c appeared in wg ready with failed-pending-eval upstream. ready output: $ready_output"
fi
echo "PASS (4/6): downstream NOT ready when upstream is failed-pending-eval"

# ── Test 5: .evaluate-X system task IS ready (system bypass) ─────────────────
# Add a fresh source and its eval task
wg add "source-d" --id source-d >add_d.log 2>&1
wg claim source-d >claim_d.log 2>&1 || true
wg fail source-d --class agent-exit-nonzero >fail_d.log 2>&1

status_d=$(wg show source-d --json 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('status','?'))" 2>/dev/null || echo "?")
if [[ "$status_d" != "failed-pending-eval" ]]; then
    loud_fail "Test 5 setup FAIL: source-d should be failed-pending-eval, got: $status_d"
fi

# Manually add a .evaluate-source-d system task that depends on source-d
wg add ".evaluate-source-d" --id ".evaluate-source-d" --after source-d --tag evaluation >add_eval_d.log 2>&1

ready_output2=$(wg ready 2>&1)
if ! echo "$ready_output2" | grep -q ".evaluate-source-d"; then
    loud_fail "Test 5 FAIL: .evaluate-source-d is NOT in wg ready despite source being failed-pending-eval. ready: $ready_output2"
fi
echo "PASS (5/6): .evaluate-X system task IS ready via system bypass"

# ── Test 6: wg list shows [e] indicator ───────────────────────────────────────
# source-d is still in failed-pending-eval
list_output=$(wg list 2>&1)
if ! echo "$list_output" | grep -q "\[e\]"; then
    # Fallback: check if "failed-pending-eval" appears anywhere in output
    if ! echo "$list_output" | grep -qi "failed.pending.eval"; then
        loud_fail "Test 6 FAIL: wg list does not show [e] indicator for FailedPendingEval. list: $list_output"
    fi
fi
echo "PASS (6/6): wg list shows [e] or failed-pending-eval for FailedPendingEval tasks"

echo ""
echo "PASS: all 6 failed-pending-eval state machine assertions passed"
exit 0

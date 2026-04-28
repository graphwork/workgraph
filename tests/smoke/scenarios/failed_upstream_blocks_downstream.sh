#!/usr/bin/env bash
# Scenario: failed_upstream_blocks_downstream
#
# Regression for fix-failed-upstream: dependency resolution treated `failed`
# upstream as satisfied, so downstream tasks transitioned to `ready` and the
# dispatcher could spawn agents against missing/broken artifacts.
#
# Contract:
#   1. upstream=failed  → downstream must NOT appear in `wg ready` output
#   2. upstream=done    → downstream MUST appear in `wg ready` output
#
# No daemon, no LLM — pure graph + `wg ready` + `wg list`.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
cd "$scratch"

if ! wg init -x shell >init.log 2>&1; then
    loud_fail "wg init failed: $(tail -5 init.log)"
fi

# Create upstream + downstream dependency chain.
# wg add output: "Added task: <title> (<id>)"
upstream_out=$(wg add "upstream-task" 2>&1)
upstream_id=$(echo "$upstream_out" | grep "^Added task:" | grep -oP '\(\K[^)]+')
downstream_out=$(wg add "downstream-task" --after "$upstream_id" 2>&1)
downstream_id=$(echo "$downstream_out" | grep "^Added task:" | grep -oP '\(\K[^)]+')

if [[ -z "$upstream_id" || -z "$downstream_id" ]]; then
    loud_fail "could not create tasks (upstream=$upstream_id downstream=$downstream_id). upstream_out=$upstream_out downstream_out=$downstream_out"
fi

# ── Test 1: failed upstream must block downstream ──────────────────────────
wg fail "$upstream_id" --reason "simulated failure for smoke test" >fail.log 2>&1 || true

ready_output=$(wg ready 2>&1)
if echo "$ready_output" | grep -q "$downstream_id"; then
    loud_fail "downstream '$downstream_id' appeared in wg ready after upstream '$upstream_id' was failed. Output:\n$ready_output"
fi

list_output=$(wg list 2>&1)
downstream_status=$(echo "$list_output" | grep "$downstream_id" | head -1)
if echo "$downstream_status" | grep -qE '\breadyv?\b'; then
    loud_fail "downstream '$downstream_id' shows ready status in wg list after upstream failed. Line:\n$downstream_status"
fi

echo "PASS (1/2): downstream is NOT ready when upstream is failed"

# ── Test 2: done upstream must unblock downstream ──────────────────────────
# Retry upstream (failed → open), then mark it done via graph manipulation
wg retry "$upstream_id" >retry.log 2>&1 || true

# Directly mark upstream done by writing to the graph (no LLM needed)
wg done "$upstream_id" --skip-smoke >done_upstream.log 2>&1 || \
    wg done "$upstream_id" >done_upstream.log 2>&1 || true

ready_after=$(wg ready 2>&1)
if ! echo "$ready_after" | grep -q "$downstream_id"; then
    loud_fail "downstream '$downstream_id' is NOT in wg ready after upstream '$upstream_id' is done. Output:\n$ready_after"
fi

echo "PASS (2/2): downstream is ready when upstream is done"
echo "PASS: failed upstream correctly blocks downstream; done upstream correctly unblocks"
exit 0

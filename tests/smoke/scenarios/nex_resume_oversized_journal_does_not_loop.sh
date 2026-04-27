#!/usr/bin/env bash
# Scenario: nex_resume_oversized_journal_does_not_loop
#
# Repros the autohaiku 311-message resume regression at the unit-test
# level. The user-facing repro was:
#
#   - qwen3-coder model with 32k context_window
#   - 311-message chat journal
#   - resume in TUI → tight loop of 400 Bad Request to lambda01
#   - throbber spins forever, no error surfaced
#
# This scenario asserts that the three regression-bar tests pass:
#
#   - test_nex_resume_truncates_oversized_journal: synthesize a 500-msg
#     journal + 32k context window, assert load_resume_data fits within
#     `context_window * hard_ceiling_pct` AND drops messages to fit.
#
#   - test_nex_400_no_retry_loop: HTTP 400 must be classified
#     non-retryable in the OAI client. Documents the policy: 400/422
#     deterministic → 0 retries; 401/403 → 0 retries; 429 → backoff;
#     5xx → backoff.
#
#   - test_nex_429_backoff: 429 is retryable with exponential backoff
#     and respects Retry-After.
#
# These three units cover the whole regression. We run them as a smoke
# so the gate fails loudly if the next bug regresses the truncation
# path or removes the 400-no-retry classification — both of which would
# silently bring the autohaiku flood back.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

# Locate workspace root (repo root). The scenario runs from a
# .wg-worktrees/<task> dir or the main checkout — either has Cargo.toml.
repo_root=""
candidate="$HERE"
for _ in 1 2 3 4 5 6; do
    if [[ -f "$candidate/Cargo.toml" ]]; then
        repo_root="$candidate"
        break
    fi
    candidate="$(dirname "$candidate")"
done
if [[ -z "$repo_root" ]]; then
    loud_fail "could not find Cargo.toml above $HERE"
fi

cd "$repo_root"

# Run the three regression tests. cargo test runs them in parallel by
# default and prints pass/fail per test. We capture stdout/stderr so we
# can dump it on failure.
log=$(mktemp -t wg_resume_smoke.XXXXXX)
trap 'rm -f "$log"' EXIT

# cargo test takes a single positional filter, but each filter is a
# substring match. `test_nex_` matches all four target tests. Use
# `-- --nocapture` is unnecessary; we just want per-test pass lines,
# which require the default (non-quiet) reporter.
if ! cargo test --lib test_nex_ >"$log" 2>&1
then
    loud_fail "wg-nex-resume-311 regression tests failed:
$(tail -40 "$log")"
fi

# Insist that each of the four named regression tests passed.
required=(
    test_nex_resume_truncates_oversized_journal
    test_nex_resume_no_truncation_when_fits
    test_nex_400_no_retry_loop
    test_nex_429_backoff
)
for t in "${required[@]}"; do
    if ! grep -qE "::${t} \.\.\. ok" "$log"; then
        loud_fail "regression test ${t} did not pass; cargo output:
$(cat "$log")"
    fi
done

echo "PASS: nex resume + retry-classification regressions stay fixed"
exit 0

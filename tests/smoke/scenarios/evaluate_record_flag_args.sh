#!/usr/bin/env bash
# Scenario: evaluate_record_flag_args
#
# Regression: `wg evaluate record` requires `--task <ID> --score <N> --source <S>`
# flag-style args. The inline-eval wrapper script in
# src/commands/service/coordinator.rs::build_inline_eval_script previously passed
# these positionally:
#   wg evaluate record <task-id> <score> --source system ...
# which clap rejected with `error: unexpected argument '<task-id>' found`,
# silently dropping every FLIP / agency evaluation result.
#
# This scenario asserts:
#   1. The flag-style form is accepted and writes an evaluation file.
#   2. The legacy positional form is still rejected (so we notice if the CLI
#      drifts back).
#
# Fast (no daemon, no LLM), deterministic.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
cd "$scratch"

if ! wg init -x shell >init.log 2>&1; then
    loud_fail "wg init failed during smoke setup: $(tail -5 init.log)"
fi

if ! wg add "Evaluate record smoke target" --id smoke-record-target >add.log 2>&1; then
    loud_fail "wg add failed: $(tail -5 add.log)"
fi

# 1. Flag-style form must succeed and write an evaluation file.
if ! wg evaluate record \
        --task smoke-record-target \
        --score 0.42 \
        --source manual \
        --notes "smoke: flag-style accepted" \
        >record.log 2>&1; then
    loud_fail "flag-style 'wg evaluate record --task --score --source' failed:\n$(cat record.log)"
fi

eval_dir=""
for cand in .wg/agency/evaluations .workgraph/agency/evaluations; do
    if [[ -d "$scratch/$cand" ]]; then
        eval_dir="$scratch/$cand"
        break
    fi
done
if [[ -z "$eval_dir" ]]; then
    loud_fail "no evaluations dir under .wg/ or .workgraph/ after record"
fi

if ! ls "$eval_dir"/eval-smoke-record-target-*.json >/dev/null 2>&1; then
    loud_fail "no eval-smoke-record-target-*.json in $eval_dir after flag-style record:\n$(ls -la "$eval_dir")"
fi

# 2. Positional form must be rejected — assert the exact error fragment users
#    reported in autohaiku, so a CLI revert here would re-trigger a FAIL.
if wg evaluate record smoke-record-target 0.42 --source manual >posrec.log 2>&1; then
    loud_fail "positional 'wg evaluate record <id> <score>' was accepted; CLI contract regressed:\n$(cat posrec.log)"
fi
if ! grep -q "unexpected argument 'smoke-record-target' found" posrec.log; then
    loud_fail "positional form failed but with unexpected error text:\n$(cat posrec.log)"
fi

echo "PASS: wg evaluate record accepts --task/--score/--source and rejects positional"
exit 0

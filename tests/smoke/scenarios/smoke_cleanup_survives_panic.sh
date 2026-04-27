#!/usr/bin/env bash
# Scenario: smoke_cleanup_survives_panic
#
# Pins the regression behind smoke-tests-leak: production smoke runs left
# 70+ orphaned `wg service daemon` processes and 200+ /tmp scratch dirs
# behind because per-scenario `trap` handlers did not fire on SIGKILL /
# panic / signal. The defense-in-depth that this scenario protects:
#
#   1. wg_smoke_sweep finds and kills `wg service daemon` processes whose
#      `--dir` lives under the smoke root, even after the parent shell that
#      spawned them died abruptly (no trap, no atexit).
#   2. wg_smoke_sweep removes leftover scratch dirs under the root.
#
# Strategy:
#   * Spawn a child bash that initialises a workgraph dir and starts a real
#     `wg service daemon`, then SIGKILLs itself before its trap can run.
#     The daemon survives, re-parented to init.
#   * Confirm pre-condition: daemon PID is alive, scratch dir exists.
#   * Run wg_smoke_sweep against the same root.
#   * Assert: daemon is dead and the scratch dir is gone.
#
# We run the leaked daemon under a private sub-root so we never touch
# fixtures that other parallel scenarios may be using.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

# Sub-root unique to this scenario so the sweep we drive only reaches
# fixtures we own. The sub_root is itself under the global smoke root, so
# the central wg_smoke_cleanup trap will reap whatever survives.
mkdir -p "$(wg_smoke_root)"
sub_root="$(mktemp -d "$(wg_smoke_root)/leak_under_test.XXXXXX")"
register_scratch "$sub_root"

leak_log="$sub_root/leak.log"

# Run a child bash that spawns the daemon and then SIGKILLs itself.
# `bash -c '... ; kill -KILL $$'` makes $$ resolve to the child's PID.
# We disable the helper's trap inside the child by overwriting EXIT; the
# whole point is to simulate the trap NOT firing.
WG_SMOKE_ROOT="$sub_root" WG_SMOKE_SCENARIO="leakchild" \
    bash -c '
        set -u
        # shellcheck disable=SC1090
        . "$1"
        require_wg
        # Defeat the helper trap so this child mimics a panic/SIGKILL with
        # no cleanup. The whole regression is "trap did not fire".
        trap - EXIT INT TERM HUP
        scratch=$(make_scratch)
        cd "$scratch"
        wg init -x shell >init.log 2>&1 \
            || wg init -x claude >init.log 2>&1 \
            || { echo "INIT_FAILED" >&2; exit 1; }
        # Spawn the daemon directly (bypasses start_wg_daemon to keep the
        # child surface minimal). Wait for state.json so the parent can
        # read the PID.
        ( wg service start --max-agents 0 --no-chat-agent >daemon.log 2>&1 ) &
        wrap_pid=$!
        wg_dir=""
        for cand in .wg .workgraph; do
            if [[ -d "$scratch/$cand" ]]; then
                wg_dir="$scratch/$cand"
                break
            fi
        done
        for _ in $(seq 1 60); do
            if [[ -f "$wg_dir/service/state.json" ]]; then
                break
            fi
            sleep 0.2
        done
        # Echo the canonical daemon PID for the parent to pick up.
        if ! grep -oE "\"pid\"[[:space:]]*:[[:space:]]*[0-9]+" \
                "$wg_dir/service/state.json" 2>/dev/null \
                | head -1 | grep -oE "[0-9]+\$"; then
            echo "NO_PID_IN_STATE" >&2
            exit 1
        fi
        wait "$wrap_pid" 2>/dev/null || true
        # Now the regression simulation: SIGKILL ourselves.
        kill -KILL $$
    ' _ "$HERE/_helpers.sh" >"$leak_log" 2>&1 || true

leaked_pid=$(grep -oE '^[0-9]+$' "$leak_log" | tail -1 || true)
if [[ -z "$leaked_pid" ]]; then
    loud_fail "child shell did not report a daemon PID. log:
$(cat "$leak_log")"
fi

# Pre-condition: leaked daemon must be alive.
if ! kill -0 "$leaked_pid" 2>/dev/null; then
    loud_fail "expected leaked daemon $leaked_pid to be alive after child SIGKILL — child shell may have failed to spawn the daemon. log:
$(cat "$leak_log")"
fi
sub_dirs_before=$(find "$sub_root" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)
if [[ "$sub_dirs_before" -lt 1 ]]; then
    loud_fail "expected at least 1 leaked scratch under $sub_root, got $sub_dirs_before"
fi

# The actual regression bar: wg_smoke_sweep must reap both the daemon
# (re-parented to init, no parent-child relationship to us) and the dir.
WG_SMOKE_ROOT="$sub_root" wg_smoke_sweep

# Assertion 1: leaked daemon is dead.
sleep 0.2
if kill -0 "$leaked_pid" 2>/dev/null; then
    sleep 1
    if kill -0 "$leaked_pid" 2>/dev/null; then
        loud_fail "wg_smoke_sweep left leaked daemon $leaked_pid alive"
    fi
fi

# Assertion 2: scratch dirs under sub_root are gone.
sub_dirs_after=$(find "$sub_root" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)
if [[ "$sub_dirs_after" -gt 0 ]]; then
    leftover=$(find "$sub_root" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | head -5)
    loud_fail "wg_smoke_sweep left $sub_dirs_after dir(s) under $sub_root, e.g.:
$leftover"
fi

echo "PASS: cleanup survives mid-test SIGKILL — wg_smoke_sweep reaped daemon $leaked_pid and $sub_dirs_before leaked scratch dir(s)"
exit 0

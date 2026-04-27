#!/usr/bin/env bash
# Scenario: dispatcher_boot_no_orphan_supervisor
#
# Bug A regression check: after `wg init` and `wg service start`, the
# registry must not contain an "orphan supervisor" / ghost coordinator task.
#
# We boot a fresh project, start the dispatcher with --no-coordinator-agent
# (so we are testing scaffold behaviour, not LLM behaviour), let it settle,
# then read .workgraph/service/registry.json and assert no agent entry has
# status=orphan / role=supervisor for a non-existent task.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
trap 'rm -rf "$scratch"' EXIT
cd "$scratch"

if ! wg init -x shell >init.log 2>&1; then
    # Older builds may not support shell executor; fall back to claude scaffold
    if ! wg init -x claude >init.log 2>&1; then
        loud_fail "wg init failed: $(tail -5 init.log)"
    fi
fi

# --no-chat-agent (legacy alias --no-coordinator-agent) skips spawning the
# chat agent so we are testing scaffold behaviour, not LLM behaviour.
wg service start --max-agents 0 --no-chat-agent >daemon.log 2>&1 &
daemon_pid=$!
trap 'kill_tree "$daemon_pid"; rm -rf "$scratch"' EXIT

# Discover the canonical workgraph directory (.wg or legacy .workgraph).
graph_dir=""
for cand in .wg .workgraph; do
    if [[ -d "$scratch/$cand" ]]; then
        graph_dir="$scratch/$cand"
        break
    fi
done
if [[ -z "$graph_dir" ]]; then
    loud_fail "no .wg/ or .workgraph/ directory after init"
fi

# Wait until registry exists or 10s elapses.
for _ in $(seq 1 20); do
    if [[ -f "$graph_dir/service/registry.json" ]] \
        || [[ -f "$graph_dir/service/state.json" ]]; then
        break
    fi
    sleep 0.5
done

sleep 1  # let daemon settle
registry="$graph_dir/service/registry.json"
if [[ ! -f "$registry" ]]; then
    # No registry yet means no orphan — which is what we want. Pass.
    echo "PASS: dispatcher booted, no registry (no orphan supervisor)"
    exit 0
fi

# Look for ghost / orphan / supervisor entries with empty or missing task_id.
if grep -qiE '"status"\s*:\s*"orphan"' "$registry"; then
    loud_fail "registry contains orphan agent entry. registry.json tail:\n$(tail -30 "$registry")"
fi
if grep -qE '"task_id"\s*:\s*"\.coordinator-[0-9]+"' "$registry" \
    && ! grep -qE '"status"\s*:\s*"alive"' "$registry"; then
    loud_fail "registry has stale .coordinator-N entry without alive status:\n$(tail -30 "$registry")"
fi

echo "PASS: registry contains no orphan supervisor entries"
exit 0

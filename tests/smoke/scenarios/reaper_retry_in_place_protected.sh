#!/usr/bin/env bash
# Scenario: reaper_retry_in_place_protected
#
# Pins reaper-edge-case: `wg sweep --reap-targets` must NOT remove the
# `target/` directory of a worktree that is being actively reused by a
# `wg retry`-in-place agent. The original-owner registry entry shows
# dead, but a different agent's registry entry points at the same
# worktree_path and is alive.
#
# Without the fix, the reaper looked up liveness by directory name only,
# so the live retry agent's build artefacts were silently yanked from
# under it (forcing a slow cargo rebuild on resume).
#
# Strategy:
#   1. Initialise a workgraph project.
#   2. Create `.wg-worktrees/agent-A/` with a fake `target/`.
#   3. Hand-craft `service/registry.json` with two agents:
#        - agent-A: PID 0x7FFFFFFE (definitely dead), Failed,
#                   worktree_path = .wg-worktrees/agent-A
#        - agent-B: our own PID (alive), Working, fresh heartbeat,
#                   worktree_path = .wg-worktrees/agent-A
#   4. Run `wg sweep --reap-targets`.
#   5. Assert `.wg-worktrees/agent-A/target` still exists.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch="$(make_scratch)"
cd "$scratch" || loud_fail "could not cd into scratch $scratch"

# A bare-minimum git repo is required so `wg init` can find a project root.
git init -q || loud_fail "git init failed"
git config user.email smoke@example.com
git config user.name "smoke"
echo "init" > README.md
git add README.md
git commit -qm "init" || loud_fail "git commit failed"

# Use shell executor — fastest init, no model needed.
wg init -m local:test -e http://127.0.0.1:1 >init.log 2>&1 \
    || loud_fail "wg init failed: $(cat init.log)"

mkdir -p .wg-worktrees/agent-A/target/debug
dd if=/dev/zero of=.wg-worktrees/agent-A/target/debug/artifact.bin bs=1024 count=64 \
    >/dev/null 2>&1 || loud_fail "dd populate target failed"

now="$(date -u +%Y-%m-%dT%H:%M:%S.%6NZ)"
wt_abs="$scratch/.wg-worktrees/agent-A"
my_pid="$$"

# Locate the workgraph directory `wg init` created. Tested fixture
# layouts use `.wg` (newer init); fall back to `.workgraph` (older init).
if [ -d .wg ]; then
    wg_dir=".wg"
elif [ -d .workgraph ]; then
    wg_dir=".workgraph"
else
    loud_fail "could not find workgraph dir after init: $(ls -la)"
fi

# Hand-craft the registry. agent-A is the dead original owner (matches
# the directory name). agent-B is the live retry-in-place occupant —
# different ID, same worktree_path, our shell's PID, fresh heartbeat.
mkdir -p "$wg_dir/service"
cat > "$wg_dir/service/registry.json" <<EOF
{
  "agents": {
    "agent-A": {
      "id": "agent-A",
      "pid": 2147483646,
      "task_id": "shared-task",
      "executor": "claude",
      "started_at": "$now",
      "last_heartbeat": "$now",
      "status": "failed",
      "output_file": "/tmp/a.log",
      "worktree_path": "$wt_abs"
    },
    "agent-B": {
      "id": "agent-B",
      "pid": $my_pid,
      "task_id": "shared-task",
      "executor": "claude",
      "started_at": "$now",
      "last_heartbeat": "$now",
      "status": "working",
      "output_file": "/tmp/b.log",
      "worktree_path": "$wt_abs"
    }
  },
  "next_agent_id": 3
}
EOF

# Sanity check the precondition.
[ -d .wg-worktrees/agent-A/target ] \
    || loud_fail "precondition failed: target/ missing before reap"

# Now invoke the reaper. With the bug, this would remove target/.
wg sweep --reap-targets >sweep.log 2>&1 \
    || loud_fail "wg sweep --reap-targets crashed: $(cat sweep.log)"

# THE assertion: target/ must survive because agent-B (live) occupies the
# worktree, even though agent-A (whose ID matches the dir name) is dead.
if [ ! -d .wg-worktrees/agent-A/target ]; then
    echo "----- sweep.log -----" 1>&2
    cat sweep.log 1>&2
    echo "----- registry.json -----" 1>&2
    cat $wg_dir/service/registry.json 1>&2
    loud_fail "REGRESSION: reaper removed target/ from a worktree occupied by a live retry agent"
fi

# Negative-control: if we mark agent-B dead, target/ SHOULD now be reaped.
sed -i 's/"status": "working"/"status": "dead"/' $wg_dir/service/registry.json
# Also invalidate the PID so is_process_alive returns false.
sed -i "s/\"pid\": $my_pid/\"pid\": 2147483645/" $wg_dir/service/registry.json

wg sweep --reap-targets >sweep2.log 2>&1 \
    || loud_fail "wg sweep --reap-targets (negative control) crashed: $(cat sweep2.log)"

if [ -d .wg-worktrees/agent-A/target ]; then
    echo "----- sweep2.log -----" 1>&2
    cat sweep2.log 1>&2
    loud_fail "negative control failed: target/ should be reaped when no live agent occupies the worktree"
fi

echo "OK: live retry-in-place worktree protected; dead worktree reaped"
exit 0

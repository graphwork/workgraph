#!/usr/bin/env bash
# Scenario: wg_done_refuses_uncommitted_worktree
#
# Regression for wg-done-silent (2026-04-28): when the agent's worktree
# branch had 0 commits ahead of main but staged-but-uncommitted tracked
# files, `wg done` returned NoCommits, marked the task done, and cleaned up
# the worktree — silently dropping the agent's work. This was the actual
# root cause behind the "AGENTS.md never landed on main" mystery.
#
# The fix: refuse `wg done` with a non-zero exit and an actionable error
# message naming the uncommitted file. The task must NOT transition to Done
# and the file must NOT appear on main.
#
# Asserts (all of):
#   (a) `wg done` exits non-zero
#   (b) the error mentions the uncommitted file name
#   (c) the file is NOT on `main`
#   (d) the task status is NOT `done`

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
project_root="$scratch/project"
mkdir -p "$project_root"

# ── Init a real git repo with `main` as the default branch ────────────
git init -b main "$project_root" >/dev/null 2>&1 \
    || loud_fail "git init failed in $project_root"

(
    cd "$project_root"
    git config user.email "smoke@test" >/dev/null
    git config user.name "Smoke" >/dev/null
    echo "initial" > README.md
    git add README.md >/dev/null
    git commit -m "initial" >/dev/null
) || loud_fail "git initial commit setup failed"

# ── Create a worktree exactly like the dispatcher would ───────────────
agent_id="agent-smoke-uncommitted"
task_id="smoke-task"
branch="wg/${agent_id}/${task_id}"
worktree_dir="$project_root/.wg-worktrees/$agent_id"

mkdir -p "$(dirname "$worktree_dir")"
( cd "$project_root" && git worktree add "$worktree_dir" -b "$branch" HEAD ) \
    >/dev/null 2>&1 \
    || loud_fail "git worktree add failed"

# ── Initialise a workgraph dir under the project ──────────────────────
wg_dir="$project_root/.workgraph"
mkdir -p "$wg_dir"
cat >"$wg_dir/graph.jsonl" <<EOF
{"kind":"task","id":"${task_id}","title":"Smoke test","status":"in-progress","created_at":"2026-04-28T00:00:00+00:00"}
EOF

# ── Stage but do NOT commit a tracked file in the worktree ────────────
staged_file="LOST_WORK.md"
echo "agent prepared this work but never committed" > "$worktree_dir/$staged_file"
( cd "$worktree_dir" && git add "$staged_file" ) \
    >/dev/null 2>&1 \
    || loud_fail "git add in worktree failed"

# Sanity: the porcelain output must list LOST_WORK.md as staged.
if ! ( cd "$worktree_dir" && git status --porcelain ) | grep -q "$staged_file"; then
    loud_fail "test setup wrong: expected $staged_file to be staged in worktree"
fi

# ── Run `wg done` with the worktree env vars the dispatcher would set ─
# Unset WG_AGENT_ID entirely (NOT empty-string) so --skip-smoke is
# permitted: done.rs treats `is_agent` via `std::env::var("WG_AGENT_ID")
# .is_ok()`, which returns true for an empty string. We are the human
# harness here, and we don't want to recurse into the smoke gate.
done_log="$scratch/wg-done.log"
unset WG_AGENT_ID
unset WG_SMOKE_AGENT_OVERRIDE
set +e
WG_WORKTREE_PATH="$worktree_dir" \
WG_BRANCH="$branch" \
WG_PROJECT_ROOT="$project_root" \
    wg --dir "$wg_dir" done "$task_id" --skip-smoke \
    >"$done_log" 2>&1
done_exit=$?
set -e

# ── Assertion (a): non-zero exit ──────────────────────────────────────
if [[ $done_exit -eq 0 ]]; then
    loud_fail "wg done exited 0 despite uncommitted staged worktree changes — bug regressed.
done.log:
$(cat "$done_log")"
fi

# ── Assertion (b): error names the uncommitted file ───────────────────
if ! grep -q "$staged_file" "$done_log"; then
    loud_fail "wg done error did not name the uncommitted file '$staged_file'.
done.log:
$(cat "$done_log")"
fi

# ── Assertion (c): file is NOT on main ────────────────────────────────
if ( cd "$project_root" && git ls-tree main -- "$staged_file" ) | grep -q "$staged_file"; then
    loud_fail "$staged_file leaked onto main despite refusal — bug regressed.
git ls-tree output:
$( cd "$project_root" && git ls-tree main -- "$staged_file" )"
fi

# ── Assertion (d): task status is NOT done ────────────────────────────
status_field=$(grep -oE '"status"[[:space:]]*:[[:space:]]*"[^"]*"' "$wg_dir/graph.jsonl" \
    | head -1 \
    | sed -E 's/.*"status"[[:space:]]*:[[:space:]]*"([^"]*)".*/\1/')

if [[ "$status_field" == "done" ]]; then
    loud_fail "task '$task_id' was marked done despite the refusal — bug regressed.
graph.jsonl row:
$(grep "\"$task_id\"" "$wg_dir/graph.jsonl")"
fi

if [[ "$status_field" == "pending-validation" || "$status_field" == "pending-eval" ]]; then
    loud_fail "task '$task_id' was moved to $status_field despite the refusal — bug partially regressed.
graph.jsonl row:
$(grep "\"$task_id\"" "$wg_dir/graph.jsonl")"
fi

echo "PASS: wg done refused uncommitted worktree, file did not land on main, task stayed in-progress"
exit 0

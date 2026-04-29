#!/usr/bin/env bash
# Scenario: codex_cli_fresh_init_runtime
#
# Regression (fix-wg-init): `wg init --route codex-cli` produces a config
# file with codex:gpt-5.5 set correctly, but the resulting project was
# completely non-functional at runtime when the user pointed
# `WG_DIR=<project_root>` at it (the natural mistake — users think
# WG_DIR points at their project, not at the .wg subdir).
#
# Five symptoms, ALL traced to one cause: `resolve_workgraph_dir` treated
# `WG_DIR=<project_root>` literally as the workgraph dir, but `wg init`
# writes everything under `<project_root>/.wg/`. So the dispatcher looked
# for graph.jsonl, service/, agency/, and config.toml directly under the
# project root and either fell back to global ~/.wg (claude:opus) or
# created sibling service/ + bogus graph paths.
#
# Fix: `descend_into_wg_subdir_if_project_root` — when WG_DIR (or --dir)
# points at a directory that contains a `.wg` or `.workgraph` child,
# descend into it. Skips descent when the path basename is already
# `.wg`/`.workgraph` or when the path itself contains `graph.jsonl` (so
# legacy users who already set WG_DIR=<wg_dir> directly are unaffected).
#
# This smoke replicates the user's verbatim repro:
#   WG_DIR=<project_root> wg agent list
#   WG_DIR=<project_root> wg service start
#
# and asserts:
#   1. dispatcher loads project [dispatcher].model = codex:gpt-5.5
#   2. wg agent list finds the seeded default agent
#   3. service runtime files land at <project>/.wg/service/, NOT
#      <project>/service/ (sibling)
#   4. graph watcher path is <project>/.wg/graph.jsonl
#   5. no "Failed to load graph for task-aware reaping" tick errors
#   6. spawn-task → handler chain carries WG_EXECUTOR_TYPE=codex and
#      WG_MODEL=codex:gpt-5.5 through to the codex-handler invocation

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)

# Isolate from any user-level workgraph config. Critically, seed the
# global config with claude:opus — this is the trap that masked Bug 1:
# when the dispatcher couldn't find project config under WG_DIR=<root>,
# it silently fell back to this global file.
fake_home="$scratch/home"
mkdir -p "$fake_home/.config/workgraph"
cat >"$fake_home/.config/workgraph/config.toml" <<'EOF'
[agent]
model = "claude:opus"

[dispatcher]
model = "claude:opus"
EOF

project="$scratch/proj"
mkdir -p "$project"

run_wg() {
    env -u WG_EXECUTOR_TYPE -u WG_MODEL -u WG_TIER -u WG_AGENT_ID -u WG_TASK_ID -u WG_DIR \
        HOME="$fake_home" XDG_CONFIG_HOME="$fake_home/.config" \
        wg "$@"
}

# Same as run_wg but with WG_DIR=<project_root> — the user's bug-trigger.
run_wg_with_root() {
    env -u WG_EXECUTOR_TYPE -u WG_MODEL -u WG_TIER -u WG_AGENT_ID -u WG_TASK_ID \
        HOME="$fake_home" XDG_CONFIG_HOME="$fake_home/.config" \
        WG_DIR="$project" \
        wg "$@"
}

cd "$project"

# ── Step 1: wg init --route codex-cli (with full agency to test Bug 2) ────────
if ! run_wg init --route codex-cli >init.log 2>&1; then
    loud_fail "wg init --route codex-cli failed: $(tail -10 init.log)"
fi

wg_dir="$project/.wg"
if [[ ! -d "$wg_dir" ]]; then
    loud_fail "no .wg/ directory after wg init"
fi

# ── Step 2 (Bug 2): WG_DIR=<project_root> wg agent list finds the agent ──────
agent_out=$(run_wg_with_root agent list 2>&1) || \
    loud_fail "wg agent list failed: $agent_out"
if grep -q "No agents defined" <<<"$agent_out"; then
    loud_fail "Bug 2: WG_DIR=<project_root> wg agent list reports 'No agents defined' even though .wg/agency/cache/agents/ has files. Output:\n$agent_out"
fi
if ! grep -qE "Careful Programmer|Default" <<<"$agent_out"; then
    loud_fail "Bug 2: wg agent list output should include the seeded default agent. Output:\n$agent_out"
fi

# ── Step 3: WG_DIR=<project_root> wg service start ────────────────────────────
( run_wg_with_root service start --max-agents 1 --no-coordinator-agent --interval 1 \
    >start.log 2>&1 ) &
wrap_pid=$!

# Wait for state.json under the proper path. If WG_DIR is treated literally,
# state.json appears at <project>/service/state.json (Bug 3) and
# wait_for_daemon_pid times out.
if ! daemon_pid=$(wait_for_daemon_pid "$wg_dir" 30); then
    wait "$wrap_pid" 2>/dev/null || true
    bogus="$project/service/state.json"
    if [[ -f "$bogus" ]]; then
        loud_fail "Bug 3: service state.json landed at $bogus (sibling to .wg) instead of $wg_dir/service/state.json. start.log:\n$(tail -20 start.log)"
    fi
    loud_fail "daemon never wrote state.json at $wg_dir/service/state.json. start.log:\n$(tail -20 start.log)"
fi
wait "$wrap_pid" 2>/dev/null || true
register_wg_daemon "$daemon_pid" "$wg_dir"

# ── Step 3a (Bug 3): no sibling service/ directory ────────────────────────────
if [[ -e "$project/service" ]]; then
    loud_fail "Bug 3: stray sibling service/ exists at $project/service. ls:\n$(ls -la "$project/service")"
fi
if [[ -e "$project/graph.jsonl" ]]; then
    loud_fail "Bug 3/4: stray sibling graph.jsonl exists at $project/graph.jsonl"
fi
if [[ -e "$project/agency" ]]; then
    loud_fail "Bug 3: stray sibling agency/ exists at $project/agency"
fi

# ── Step 4: assert daemon.log content (Bugs 1, 4, 5) ──────────────────────────
daemon_log="$wg_dir/service/daemon.log"
config_seen=false
for _ in $(seq 1 40); do
    if [[ -f "$daemon_log" ]] && grep -q "Coordinator config" "$daemon_log"; then
        config_seen=true
        break
    fi
    sleep 0.2
done
if ! $config_seen; then
    loud_fail "daemon.log missing 'Coordinator config' line within 8s. tail:\n$(tail -30 "$daemon_log" 2>/dev/null || echo '<no log>')"
fi

log_contents=$(cat "$daemon_log")

# Bug 1: dispatcher must use project codex config, not global claude.
if ! grep -qE 'executor=codex.*model=codex:gpt-5\.5' <<<"$log_contents"; then
    loud_fail "Bug 1: dispatcher did not load project [dispatcher].model = codex:gpt-5.5. Found:\n$(grep 'Coordinator config' <<<"$log_contents")"
fi
if grep -qE 'model=claude:opus' <<<"$log_contents"; then
    loud_fail "Bug 1: dispatcher fell back to global claude:opus instead of project codex:gpt-5.5"
fi

# Bug 4: graph watcher must point inside .wg.
if ! grep -qF "Graph watcher active on $wg_dir/graph.jsonl" <<<"$log_contents"; then
    loud_fail "Bug 4: graph watcher path wrong. Expected '$wg_dir/graph.jsonl'. Got:\n$(grep 'Graph watcher' <<<"$log_contents")"
fi
if grep -qF "Graph watcher active on $project/graph.jsonl" <<<"$log_contents"; then
    loud_fail "Bug 4: graph watcher pointed at sibling $project/graph.jsonl"
fi

# Bug 5: no continuous tick errors. Wait one full poll cycle, then check.
sleep 2
log_contents=$(cat "$daemon_log")
if grep -q "Failed to load graph for task-aware reaping" <<<"$log_contents"; then
    loud_fail "Bug 5: dispatcher logs 'Failed to load graph for task-aware reaping'. Tail:\n$(tail -40 "$daemon_log")"
fi

# ── Step 5: spawn-task --dry-run carries WG_EXECUTOR_TYPE/WG_MODEL through ────
# Per the validation: confirm WG_EXECUTOR_TYPE=codex and WG_MODEL=codex:gpt-5.5
# arrive at the worker process. The spawn-task subcommand is what the
# dispatcher exec()s for each worker; --dry-run prints the resolved handler
# invocation including its -m flag without actually running codex.
#
# Add a task and run spawn-task --dry-run with the env vars the dispatcher
# would have set on a real spawn.
if ! run_wg_with_root add 'smoke probe' --id smoke-probe \
        -d 'echo hello' >add.log 2>&1; then
    loud_fail "wg add smoke-probe failed: $(tail -10 add.log)"
fi

spawn_out=$(env -u WG_TIER -u WG_AGENT_ID -u WG_TASK_ID \
    HOME="$fake_home" XDG_CONFIG_HOME="$fake_home/.config" \
    WG_DIR="$project" WG_EXECUTOR_TYPE=codex WG_MODEL=codex:gpt-5.5 \
    wg spawn-task --dry-run smoke-probe 2>&1) || \
    loud_fail "wg spawn-task --dry-run failed: $spawn_out"

if grep -qE 'wg claude-handler' <<<"$spawn_out"; then
    loud_fail "Bug 1 (worker half): spawn-task dispatched to claude-handler despite WG_EXECUTOR_TYPE=codex. Output:\n$spawn_out"
fi
if ! grep -qE 'wg codex-handler' <<<"$spawn_out"; then
    loud_fail "spawn-task did not dispatch to codex-handler. Output:\n$spawn_out"
fi
if ! grep -qE -- '-m codex:gpt-5\.5' <<<"$spawn_out"; then
    loud_fail "spawn-task did not pass -m codex:gpt-5.5 to codex-handler. Output:\n$spawn_out"
fi
if grep -qE -- '-m claude:' <<<"$spawn_out"; then
    loud_fail "spawn-task used -m claude:* despite WG_MODEL=codex:gpt-5.5. Output:\n$spawn_out"
fi

echo "PASS: WG_DIR=<project_root> resolves to <project>/.wg, dispatcher uses codex:gpt-5.5, no sibling service/graph paths, worker spawn carries WG_EXECUTOR_TYPE=codex + WG_MODEL=codex:gpt-5.5"
exit 0

#!/usr/bin/env bash
# Scenario: dispatcher_codex_wins_over_agency
#
# Regression (agency-still-picks): the autohaiku fix `agency-picks-claude`
# moved a model-prefix override into `Agent::effective_executor_for_model`
# (claude → native when model has a non-Anthropic prefix). That override
# fired in `resolve_executor`'s precedence step 3 (agency-derived) BEFORE
# step 4 (`[dispatcher].executor`), so `wg init -x codex -m local:qwen3`
# was silently rewritten to native: the user's explicit codex choice was
# overridden by the agency's claude→native compatibility patch.
#
# Fix: the agency abstains for default-claude agents (no explicit
# `executor` field, no `preferred_provider`). The model-compat override
# moved into `dispatch::plan_spawn::enforce_model_compat` so it runs
# AFTER the dispatcher's executor floor — only kicking in when the
# resolved executor is genuinely claude.
#
# This smoke pins the wiring end-to-end. We do NOT need a real LLM
# endpoint — the SpawnPlan provenance line is emitted before the spawn
# process launches.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
trap 'rm -rf "$scratch"' EXIT

# Isolate from any user-level workgraph config.
fake_home="$scratch/home"
mkdir -p "$fake_home/.config/workgraph"
: >"$fake_home/.config/workgraph/config.toml"

cd "$scratch"
project="$scratch/proj"
mkdir -p "$project"
cd "$project"

run_wg() {
    env -u WG_EXECUTOR_TYPE -u WG_MODEL -u WG_TIER -u WG_AGENT_ID -u WG_TASK_ID \
        HOME="$fake_home" XDG_CONFIG_HOME="$fake_home/.config" \
        wg "$@"
}

# Initialize project with codex dispatcher executor + a local: model. The
# user's explicit `-x codex` is the contract under test: the dispatcher
# MUST honor it even though the model has a non-Anthropic prefix.
if ! run_wg init -x codex -m local:qwen3-coder \
        -e https://example.invalid/v1 \
        >init.log 2>&1; then
    loud_fail "wg init failed: $(tail -10 init.log)"
fi

graph_dir=""
for cand in .wg .workgraph; do
    if [[ -d "$project/$cand" ]]; then
        graph_dir="$project/$cand"
        break
    fi
done
if [[ -z "$graph_dir" ]]; then
    loud_fail "no .wg/ or .workgraph/ directory after init"
fi

# Seed the agency with starter primitives + default agents. The default
# Careful Programmer agent has no explicit executor / preferred_provider
# — exactly the case where the buggy version overrode the dispatcher.
if ! run_wg agency init >agency-init.log 2>&1; then
    loud_fail "wg agency init failed: $(tail -10 agency-init.log)"
fi

agent_hash=$(run_wg agent list 2>/dev/null \
    | awk '/exec:claude/ {print $1; exit}')
if [[ -z "${agent_hash:-}" ]]; then
    loud_fail "no default agent with exec:claude after wg agency init — wg agent list output:\n$(run_wg agent list 2>&1)"
fi

if ! run_wg add 'smoke probe' --id smoke-probe \
        -d 'echo hello world' \
        >add.log 2>&1; then
    loud_fail "wg add failed: $(tail -10 add.log)"
fi

if ! run_wg assign smoke-probe "$agent_hash" >assign.log 2>&1; then
    loud_fail "wg assign failed: $(tail -10 assign.log)"
fi

if ! run_wg publish smoke-probe --only >publish.log 2>&1; then
    loud_fail "wg publish failed: $(tail -10 publish.log)"
fi

# Smoke gate refuses --skip-smoke for agents (WG_AGENT_ID set). This
# scenario is invoked by the smoke harness, not from inside an agent
# process, so the override is safe; .assign-smoke-probe owns no
# scenarios so the bypass is a no-op for coverage.
if ! WG_SMOKE_AGENT_OVERRIDE=1 run_wg done .assign-smoke-probe \
        --skip-smoke >done.log 2>&1; then
    loud_fail "wg done .assign-smoke-probe failed: $(tail -10 done.log)"
fi

# Boot dispatcher in background.
run_wg service start --max-agents 1 --no-coordinator-agent --interval 1 \
    >start.log 2>&1 &
daemon_pid=$!
trap 'kill_tree "$daemon_pid"; rm -rf "$scratch"' EXIT

daemon_log="$graph_dir/service/daemon.log"

spawn_seen=false
for _ in $(seq 1 60); do
    if [[ -f "$daemon_log" ]] \
       && grep -q "smoke-probe: SpawnPlan" "$daemon_log" 2>/dev/null; then
        spawn_seen=true
        break
    fi
    sleep 0.5
done

spawn_lines=$(grep "smoke-probe: SpawnPlan" "$daemon_log" 2>/dev/null || true)

kill_tree "$daemon_pid"
trap 'rm -rf "$scratch"' EXIT

if ! $spawn_seen; then
    loud_fail "dispatcher did not emit a SpawnPlan line for smoke-probe within 30s. Tail of daemon.log:\n$(tail -40 "$daemon_log" 2>/dev/null || echo '<no daemon log>')\nstart.log:\n$(tail -10 start.log 2>/dev/null)"
fi

# Hard assertion: SpawnPlan must route through codex (the dispatcher's
# explicit `-x codex`), not native (which is what the buggy agency-side
# override would have produced).
if grep -qE 'SpawnPlan executor=native' <<<"$spawn_lines"; then
    loud_fail "dispatcher -x codex was overridden to native (agency-still-picks regression returned):\n$spawn_lines"
fi
if grep -qE 'SpawnPlan executor=claude' <<<"$spawn_lines"; then
    loud_fail "dispatcher -x codex routed through claude (worse: model-compat didn't fire either):\n$spawn_lines"
fi
if ! grep -qE 'SpawnPlan executor=codex' <<<"$spawn_lines"; then
    loud_fail "SpawnPlan executor is not codex for `-x codex` + local: model — got:\n$spawn_lines"
fi

echo "PASS: dispatcher -x codex wins over agency for local: model (SpawnPlan: $spawn_lines)"
exit 0

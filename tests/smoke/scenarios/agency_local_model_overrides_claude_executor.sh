#!/usr/bin/env bash
# Scenario: agency_local_model_overrides_claude_executor
#
# Regression (autohaiku 100% failure): the agency's effective_executor used
# to return "claude" whenever the agent's executor field was the default
# ("claude"), even when the model was a non-Anthropic spec like
# `local:qwen3-coder`. The dispatcher then handed that combo to the claude
# CLI, which has no idea what `qwen3-coder` is and 404'd every spawn.
#
# Fix: `Agent::effective_executor_for_model(model)` overrides claude →
# native (or whatever the model's provider prefix requires) when the model
# is non-Anthropic. The dispatcher passes the resolved task model into
# this method.
#
# This smoke pins the wiring end-to-end by booting the dispatcher, letting
# it observe a task assigned to a default-claude agent, and asserting on
# the SpawnPlan provenance line in the daemon log. We do NOT need a real
# LLM endpoint — the SpawnPlan log is emitted before the spawn process
# launches, so a bogus endpoint URL is sufficient.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)

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

# Initialize project with native dispatcher executor + a local: model and a
# bogus (but routable-looking) endpoint. The dispatcher's executor is native
# here, but the regression is that the AGENCY would still pick claude for
# any default agent — overriding the floor and routing through the wrong
# CLI.
if ! run_wg init -x native -m local:qwen3-coder \
        -e https://example.invalid/v1 \
        >init.log 2>&1; then
    loud_fail "wg init failed: $(tail -10 init.log)"
fi

# Seed the agency with starter primitives + default agents (one of which
# has executor=claude — the autohaiku trigger).
if ! run_wg agency init >agency-init.log 2>&1; then
    loud_fail "wg agency init failed: $(tail -10 agency-init.log)"
fi

# Pick the "Careful Programmer" default agent (executor=claude). Fall back
# to any default agent whose executor is claude.
agent_hash=$(run_wg agent list 2>/dev/null \
    | awk '/exec:claude/ {print $1; exit}')
if [[ -z "${agent_hash:-}" ]]; then
    loud_fail "no default agent with exec:claude after wg agency init — wg agent list output:\n$(run_wg agent list 2>&1)"
fi

# Add a task, manually bind the claude-default agent, and publish. We mark
# the auto-created `.assign-<task>` task done with --skip-smoke (we already
# did the assignment ourselves) so the dispatcher doesn't try to invoke a
# real LLM to pick an agent. Without this, the .assign- task would block
# our task forever against the bogus endpoint.
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

# Smoke gate refuses --skip-smoke for agents (WG_AGENT_ID set). We're not
# under an agent here, but be explicit about why this is safe: there are
# no smoke scenarios owned by `.assign-smoke-probe` since that's an
# ephemeral test task id, so the bypass is a no-op for coverage.
if ! WG_SMOKE_AGENT_OVERRIDE=1 run_wg done .assign-smoke-probe \
        --skip-smoke >done.log 2>&1; then
    loud_fail "wg done .assign-smoke-probe failed: $(tail -10 done.log)"
fi

# Boot dispatcher in background. --max-agents=1 bounds resource use.
# --no-coordinator-agent skips the chat coordinator (we only care about the
# task dispatch path). --interval 1 keeps the dispatcher loop tight enough
# that we don't wait 30s for the first tick.
#
# We can't use start_wg_daemon here because we need the env-isolated `run_wg`
# wrapper. Replicate the canonical-PID-from-state.json discipline manually.
( env -u WG_EXECUTOR_TYPE -u WG_MODEL -u WG_TIER -u WG_AGENT_ID -u WG_TASK_ID \
        HOME="$fake_home" XDG_CONFIG_HOME="$fake_home/.config" \
        wg service start --max-agents 1 --no-coordinator-agent --interval 1 \
        >start.log 2>&1 ) &
wrap_pid=$!
graph_dir=""
for cand in .wg .workgraph; do
    if [[ -d "$project/$cand" ]]; then
        graph_dir="$project/$cand"
        break
    fi
done
# graph_dir may not exist yet; wait_for_daemon_pid races state.json creation.
for _ in $(seq 1 30); do
    [[ -n "$graph_dir" ]] && break
    for cand in .wg .workgraph; do
        if [[ -d "$project/$cand" ]]; then
            graph_dir="$project/$cand"
            break
        fi
    done
    sleep 0.2
done
if [[ -z "$graph_dir" ]]; then
    loud_fail "no .wg/ or .workgraph/ directory after wg service start"
fi
if ! daemon_pid=$(wait_for_daemon_pid "$graph_dir" 30); then
    wait "$wrap_pid" 2>/dev/null || true
    loud_fail "daemon never wrote state.json. start.log:\n$(tail -20 start.log 2>/dev/null)"
fi
wait "$wrap_pid" 2>/dev/null || true
register_wg_daemon "$daemon_pid" "$graph_dir"

daemon_log="$graph_dir/service/daemon.log"

# Wait for the SpawnPlan log line to appear (or timeout). The dispatcher
# emits one provenance line per spawn attempt — that's our gate.
spawn_seen=false
for _ in $(seq 1 60); do
    if [[ -f "$daemon_log" ]] \
       && grep -q "smoke-probe: SpawnPlan" "$daemon_log" 2>/dev/null; then
        spawn_seen=true
        break
    fi
    sleep 0.5
done

# Capture the relevant log line(s). The wg_smoke_cleanup trap handles
# daemon teardown on exit — no manual kill needed.
spawn_lines=$(grep "smoke-probe: SpawnPlan" "$daemon_log" 2>/dev/null || true)

if ! $spawn_seen; then
    loud_fail "dispatcher did not emit a SpawnPlan line for smoke-probe within 30s. Tail of daemon.log ($daemon_log):\n$(tail -40 "$daemon_log" 2>/dev/null || echo '<no daemon log>')\nstart.log:\n$(tail -10 start.log 2>/dev/null)"
fi

# Hard assertion: SpawnPlan must NOT route through the claude executor.
# claude CLI cannot speak OpenAI-compat endpoints; the autohaiku failure
# was 100% spawn 404s because the agency picked claude for a local: model.
if grep -qE 'SpawnPlan executor=claude' <<<"$spawn_lines"; then
    loud_fail "agency still routes a local: model through executor=claude — autohaiku regression returned:\n$spawn_lines"
fi

# Positive assertion: native executor must be the chosen kind. (codex/
# amplifier could in principle satisfy a non-claude model spec, but for a
# `local:` prefix the canonical mapping is native.)
if ! grep -qE 'SpawnPlan executor=native' <<<"$spawn_lines"; then
    loud_fail "SpawnPlan executor is not native for a local: model — got:\n$spawn_lines"
fi

echo "PASS: agency overrides claude → native when model has local: prefix (SpawnPlan: $spawn_lines)"
exit 0

#!/usr/bin/env bash
# Scenario: codex_handler_pipes_prompt_to_stdin
#
# Regression (codex-handler-doesn): the dispatch pipeline auto-builds a
# `PromptTemplate` for built-in handlers that ship without one, but the
# gating condition in `spawn_agent_inner` (src/commands/spawn/execution.rs)
# hard-coded the list as `claude | amplifier | native` — codex was missing.
# Result: every codex agent spawn produced a run.sh that invoked `codex
# exec ...` with no `cat prompt.txt | ` prefix and no prompt.txt on disk,
# so the codex CLI sat reading stdin, got nothing, and exited 1 with
# 'No prompt provided via stdin'. The codex handler shipped to users and
# stayed broken because no smoke ever exercised it end-to-end.
#
# This scenario pins the wiring: spawn a codex task, then assert the
# generated run.sh actually feeds prompt.txt into the codex subprocess
# via stdin redirection. We do NOT call a real LLM — the assertion is
# on the dispatcher's filesystem artifacts (run.sh + prompt.txt), which
# are written before the subprocess launches.

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

if ! run_wg init -m codex:gpt-5 >init.log 2>&1; then
    loud_fail "wg init failed: $(tail -10 init.log)"
fi

if ! run_wg add 'smoke probe codex' --id smoke-probe-codex \
        -d 'echo hello world' \
        --no-place \
        >add.log 2>&1; then
    loud_fail "wg add failed: $(tail -10 add.log)"
fi

# Boot dispatcher in background using the helper that captures the canonical
# daemon PID from state.json (not $! of the wrapper). The helper also
# registers the daemon for teardown.
if ! start_wg_daemon "$project" --max-agents 1 --no-coordinator-agent --interval 1; then
    loud_fail "start_wg_daemon failed"
fi

graph_dir="$WG_SMOKE_DAEMON_DIR"
agents_root="$graph_dir/agents"

# Wait until the dispatcher writes a run.sh for an agent assigned to our
# task. We don't know the agent id ahead of time — the dispatcher mints
# agent-N and the system tasks (.assign-* etc) get spawned alongside.
agent_run_sh=""
agent_prompt_txt=""
for _ in $(seq 1 60); do
    for agent_dir in "$agents_root"/agent-*/; do
        [[ -d "$agent_dir" ]] || continue
        # Strip trailing slash from the glob expansion so absolute paths we
        # build below match the dispatcher's `cat '<no-double-slash>'` line.
        agent_dir="${agent_dir%/}"
        meta="$agent_dir/metadata.json"
        [[ -f "$meta" ]] || continue
        # Match this agent to our task id and the codex executor.
        if grep -q '"task_id": "smoke-probe-codex"' "$meta" \
           && grep -q '"executor": "codex"' "$meta"; then
            run_sh="$agent_dir/run.sh"
            if [[ -f "$run_sh" ]]; then
                agent_run_sh="$run_sh"
                agent_prompt_txt="$agent_dir/prompt.txt"
                break
            fi
        fi
    done
    [[ -n "$agent_run_sh" ]] && break
    sleep 0.5
done

if [[ -z "$agent_run_sh" ]]; then
    loud_fail "no codex agent run.sh appeared for smoke-probe-codex within 30s. agents/:
$(ls -la "$agents_root" 2>/dev/null || echo '<no agents dir>')
daemon log tail:
$(tail -20 "$graph_dir/service/daemon.log" 2>/dev/null || echo '<no daemon log>')"
fi

# Hard assertion #1: the prompt.txt sidecar exists and is non-empty.
if [[ ! -s "$agent_prompt_txt" ]]; then
    loud_fail "codex spawn did not write a non-empty prompt.txt at $agent_prompt_txt — the auto-prompt branch in spawn_agent_inner skipped codex (regression for codex-handler-doesn)"
fi

# Hard assertion #2: run.sh actually pipes prompt.txt into codex exec.
# Pattern:  cat '<absolute prompt path>' | 'codex' 'exec' ...
if ! grep -qE "cat '${agent_prompt_txt}' \| 'codex' 'exec'" "$agent_run_sh"; then
    loud_fail "codex run.sh does not pipe prompt.txt into the codex subprocess (regression for codex-handler-doesn). Expected 'cat <prompt.txt> | codex exec ...'. Actual codex line:
$(grep -E '\bcodex\b' "$agent_run_sh" || echo '<no codex line found>')"
fi

# Hard assertion #3: belt-and-braces — the run.sh must NOT invoke codex
# without a stdin pipe (the exact symptom of the original bug).
if grep -qE "^[[:space:]]*([a-zA-Z_][a-zA-Z0-9_]*=[^[:space:]]+[[:space:]])*'codex' 'exec'" "$agent_run_sh"; then
    bad_line=$(grep -nE "^[[:space:]]*([a-zA-Z_][a-zA-Z0-9_]*=[^[:space:]]+[[:space:]])*'codex' 'exec'" "$agent_run_sh" | head -1)
    loud_fail "codex run.sh has a bare 'codex exec' invocation with no upstream pipe (regression for codex-handler-doesn): $bad_line"
fi

echo "PASS: codex spawn writes prompt.txt and run.sh pipes it into 'codex exec' via stdin"
exit 0

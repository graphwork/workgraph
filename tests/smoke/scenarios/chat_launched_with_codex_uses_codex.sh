#!/usr/bin/env bash
# Scenario: chat_launched_with_codex_uses_codex
#
# Regression (chat-launched-with): launching a chat with
# `--executor codex --model codex:gpt-5` against a project whose global
# default was `claude:opus` silently spawned a claude session. Two-half
# bug:
#
#   1. `VizApp::maybe_auto_enable_chat_pty` only consulted
#      `config.coordinator.effective_executor()` — it ignored the
#      per-coordinator `CoordinatorState.executor_override` /
#      `model_override` fields written by `wg chat create`.
#   2. `spawn-task::resolve_handler` only forwarded `WG_EXECUTOR_TYPE`
#      to `plan_spawn`, so even when the daemon set `WG_MODEL=codex:gpt-5`
#      the model fell back to `[dispatcher].model` (claude:opus). The
#      codex handler then ran with `-m claude:opus` — visible to the
#      user as "asked for codex, got claude".
#
# Fix: state.rs grew `resolve_chat_pty_executor_and_model` (per-chat
# overrides win) + WG_MODEL is propagated into the auto-PTY child env;
# spawn_task.rs now reads WG_MODEL alongside WG_EXECUTOR_TYPE.
#
# This smoke pins the spawn-task half end-to-end. We DO NOT need the TUI
# loop — the daemon's actual behavior is to set both env vars before
# exec'ing `wg spawn-task .chat-N`, and that is exactly what we
# replicate. The auto-PTY half is locked by unit tests in
# `tui::viz_viewer::state::chat_pty_executor_resolution_tests`.

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

# Project default is the user's reported config: claude:opus. The whole
# point of the bug is that this default was silently winning.
if ! run_wg init --no-agency -m claude:opus >init.log 2>&1; then
    loud_fail "wg init failed: $(tail -10 init.log)"
fi

# The user's verbatim flow: pick codex + a codex model on chat creation.
if ! run_wg chat create --executor codex --model codex:gpt-5 \
        --name codextest >create.log 2>&1; then
    loud_fail "wg chat create failed: $(tail -10 create.log)"
fi

# Find the workgraph dir (`.wg` preferred, `.workgraph` legacy).
graph_dir=""
for cand in .wg .workgraph; do
    if [[ -d "$project/$cand" ]]; then
        graph_dir="$project/$cand"
        break
    fi
done
if [[ -z "$graph_dir" ]]; then
    loud_fail "no .wg/ or .workgraph/ after wg init"
fi

# Step 1: per-chat overrides land in CoordinatorState. This is the
# half-1 contract: `wg chat create` MUST persist the overrides where
# the auto-PTY launcher can find them.
state_file="$graph_dir/service/coordinator-state-0.json"
if [[ ! -f "$state_file" ]]; then
    loud_fail "no coordinator-state-0.json after wg chat create. ls service/:\n$(ls "$graph_dir/service" 2>&1)"
fi
if ! grep -q '"executor_override": "codex"' "$state_file"; then
    loud_fail "executor_override not persisted. state file:\n$(cat "$state_file")"
fi
if ! grep -q '"model_override": "codex:gpt-5"' "$state_file"; then
    loud_fail "model_override not persisted. state file:\n$(cat "$state_file")"
fi

# Step 2: spawn-task with the env vars the daemon supervisor sets on
# spawn. This is the half-2 contract: the per-chat plan resolved by the
# daemon (executor=codex, model=codex:gpt-5) MUST survive into the
# spawn-task → plan_spawn → handler-exec chain.
spawn_out=$(env -u WG_TIER -u WG_AGENT_ID -u WG_TASK_ID \
    HOME="$fake_home" XDG_CONFIG_HOME="$fake_home/.config" \
    WG_DIR="$graph_dir" WG_EXECUTOR_TYPE=codex WG_MODEL=codex:gpt-5 \
    wg spawn-task --dry-run .chat-0 2>&1) || \
    loud_fail "wg spawn-task --dry-run failed: $spawn_out"

# Hard assertions.
if grep -qE 'wg claude-handler' <<<"$spawn_out"; then
    loud_fail "spawn-task dispatched to claude-handler despite WG_EXECUTOR_TYPE=codex (chat-launched-with regression returned):\n$spawn_out"
fi
if ! grep -qE 'wg codex-handler' <<<"$spawn_out"; then
    loud_fail "spawn-task did not dispatch to codex-handler. Output:\n$spawn_out"
fi
if grep -qE -- '-m claude:opus' <<<"$spawn_out"; then
    loud_fail "spawn-task used -m claude:opus despite WG_MODEL=codex:gpt-5 (model-half of chat-launched-with regression):\n$spawn_out"
fi
if ! grep -qE -- '-m codex:gpt-5' <<<"$spawn_out"; then
    loud_fail "spawn-task did not pass -m codex:gpt-5 to codex-handler. Output:\n$spawn_out"
fi

echo "PASS: chat created with --executor codex --model codex:gpt-5 dispatches codex-handler -m codex:gpt-5 (not claude:opus)"
exit 0

#!/usr/bin/env bash
# Scenario: agency_codex_override_routes_to_codex
#
# Pins fix-agency-tasks: when a project sets `[models.evaluator]` /
# `[models.flip_*]` / `[models.assigner]` to a `codex:*` model (the
# layout written by `wg init --route codex-cli`), the inline spawn of
# an agency one-shot task MUST register the agent with `executor="codex"`
# and `model="codex:*"` — NOT the legacy claude pin. The runtime LLM
# call routes via `handler_for_model` to the codex CLI; the registry
# display reflects what is actually invoked.
#
# Pre-fix the spawn site hard-coded `executor="claude"` /
# `model="claude:haiku"` regardless of any per-role override, AND
# `run_lightweight_llm_call` collapsed `codex:` → `oai-compat` and
# tried HTTP — which 404'd without an OPENAI_API_KEY for users on
# the codex CLI route.
#
# Tested via the .evaluate-* path (inline-spawn). The .assign-* path
# is exercised in-process by the dispatcher's auto_assign loop and
# does not register a separate agent in the registry, so this scenario
# focuses on .evaluate-* which always goes through `spawn_eval_inline`.
#
# Fast (no real LLM call) — checks only the registry/metadata fields
# written BEFORE the bash script's LLM call would run.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
cd "$scratch"

# Init the codex-cli route — writes [models.evaluator] /
# [models.flip_inference] / [models.flip_comparison] / [models.assigner]
# all pointing at codex:gpt-5.4-mini.
if ! wg init --route codex-cli >init.log 2>&1; then
    loud_fail "wg init --route codex-cli failed: $(tail -10 init.log)"
fi

config_path=".wg/config.toml"
if [[ ! -f "$config_path" ]]; then
    config_path=".workgraph/config.toml"
fi
if ! grep -A1 'models\.evaluator' "$config_path" 2>/dev/null \
        | grep -q 'codex:'; then
    loud_fail "wg init --route codex-cli did not write [models.evaluator] codex override. config:
$(cat "$config_path" 2>/dev/null)"
fi

if ! wg add "Smoke parent target" --id smoke-codex-target >add.log 2>&1; then
    loud_fail "wg add parent failed: $(tail -5 add.log)"
fi

# Manually-pinned `.evaluate-*` task — same shape as
# agency_inline_spawn_registers_executor_claude.sh.
if ! wg add "Inline eval smoke" \
        --id .evaluate-smoke-codex-target \
        --tag evaluation \
        --exec "true" \
        >evaladd.log 2>&1; then
    loud_fail "wg add .evaluate-* failed: $(tail -5 evaladd.log)"
fi
if ! wg edit .evaluate-smoke-codex-target --exec-mode bare >editmode.log 2>&1; then
    loud_fail "wg edit --exec-mode bare failed: $(tail -5 editmode.log)"
fi

start_wg_daemon "$scratch" --max-agents 2

wg_dir="$WG_SMOKE_DAEMON_DIR"
registry="$wg_dir/service/registry.json"

# Wait up to 15s for the .evaluate-* agent to register.
agent_id=""
for i in $(seq 1 75); do
    if [[ -f "$registry" ]]; then
        agent_id=$(python3 -c "
import json, sys
try:
    r = json.load(open('$registry'))
except Exception:
    sys.exit(0)
for aid, info in (r.get('agents') or {}).items():
    if info.get('task_id') == '.evaluate-smoke-codex-target':
        print(aid)
        break
" 2>/dev/null || true)
        if [[ -n "$agent_id" ]]; then
            break
        fi
    fi
    sleep 0.2
done

if [[ -z "$agent_id" ]]; then
    loud_fail "no agent registered for .evaluate-smoke-codex-target after 15s. registry:
$(cat "$registry" 2>/dev/null | head -50)"
fi

executor=$(python3 -c "
import json
r = json.load(open('$registry'))
print(r['agents']['$agent_id'].get('executor', ''))
" 2>/dev/null || true)

if [[ "$executor" != "codex" ]]; then
    loud_fail "agent $agent_id for .evaluate-* registered with executor='$executor' (expected 'codex' — explicit [models.evaluator] override should win). Full agent record:
$(python3 -c "import json; print(json.dumps(json.load(open('$registry'))['agents']['$agent_id'], indent=2))" 2>/dev/null)"
fi

model=$(python3 -c "
import json
r = json.load(open('$registry'))
print(r['agents']['$agent_id'].get('model', ''))
" 2>/dev/null || true)

if [[ "$model" != codex:* ]]; then
    loud_fail "agent $agent_id for .evaluate-* registered with model='$model' (expected 'codex:...')."
fi

metadata="$wg_dir/agents/$agent_id/metadata.json"
if [[ ! -f "$metadata" ]]; then
    loud_fail "no metadata.json for $agent_id at $metadata"
fi

meta_executor=$(python3 -c "
import json
print(json.load(open('$metadata')).get('executor', ''))
" 2>/dev/null || true)

if [[ "$meta_executor" != "codex" ]]; then
    loud_fail "$metadata reports executor='$meta_executor' (expected 'codex')"
fi

meta_model=$(python3 -c "
import json
print(json.load(open('$metadata')).get('model', ''))
" 2>/dev/null || true)

if [[ "$meta_model" != codex:* ]]; then
    loud_fail "$metadata reports model='$meta_model' (expected 'codex:...')"
fi

echo "PASS: .evaluate-* inline spawn honored [models.evaluator]=codex:* override (agent=$agent_id, executor=$executor, model=$model)"
exit 0

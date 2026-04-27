#!/usr/bin/env bash
# Scenario: chat_create_via_ipc_works
#
# Boot a fresh project + dispatcher, send a single `wg chat 'hi'` and assert
# we get a non-error coordinator response within the timeout. This is the
# first thing the user does after `wg init` — if it doesn't work, nothing
# does.
#
# We use the `claude` executor by default because that is the most common
# user setup. If the claude CLI is missing we loud-skip (exit 77) — the
# behaviour we are protecting requires a real chat agent.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

if ! command -v claude >/dev/null 2>&1; then
    loud_skip "CLAUDE CLI MISSING" "claude CLI not on PATH; run 'wg setup --provider anthropic'"
fi
if [[ -z "${OPENROUTER_API_KEY:-}" ]] && [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
    loud_skip "NO LLM CREDENTIALS" "neither OPENROUTER_API_KEY nor ANTHROPIC_API_KEY set — chat agent cannot reach a model"
fi

scratch=$(make_scratch)
cd "$scratch"

if ! wg init -x claude >init.log 2>&1; then
    loud_fail "wg init -x claude failed: $(tail -5 init.log)"
fi

start_wg_daemon "$scratch" --max-agents 1
graph_dir="$WG_SMOKE_DAEMON_DIR"

# Single 'hi' must succeed within 30s.
if ! timeout 60 wg chat "hi" --timeout 30 >chat.log 2>&1; then
    loud_fail "wg chat 'hi' failed: $(tail -10 chat.log)"
fi
if grep -qiE 'role=system-error|role=error|status:.*404|HTTP/.*404' chat.log; then
    loud_fail "wg chat 'hi' returned an error response: $(tail -10 chat.log)"
fi

# Positive assertion: response must be non-trivial. Empty/whitespace-only
# output suggests a silent stub or a dropped pipe — that's the eyeball-gate
# regression we are explicitly closing.
chat_chars=$(tr -d '[:space:]' <chat.log | wc -c)
if [[ "$chat_chars" -lt 20 ]]; then
    loud_fail "wg chat 'hi' response was only $chat_chars chars (<20). Suggests stub/empty path. Output:\n$(cat chat.log)"
fi

# Best-effort: if an outbox file exists, also assert >=1 coordinator entry.
outbox=$(find "$graph_dir/chat" -maxdepth 3 -name 'outbox.jsonl' 2>/dev/null | head -1)
coord_count=0
if [[ -n "$outbox" ]] && [[ -f "$outbox" ]]; then
    coord_count=$(grep -c '"role"\s*:\s*"coordinator"' "$outbox" 2>/dev/null || echo 0)
    if [[ "$coord_count" -lt 1 ]]; then
        loud_fail "outbox $outbox has 0 coordinator responses. Tail:\n$(tail -5 "$outbox")"
    fi
fi

echo "PASS: wg chat 'hi' returned $chat_chars chars (outbox: $coord_count coordinator responses)"
exit 0

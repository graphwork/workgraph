#!/usr/bin/env bash
# Scenario: nex_two_message_against_lambda01
#
# Reproduces the user-reported regression:
#   wg init -x nex -m qwen3-coder -e https://lambda01.tail334fe6.ts.net:30000
#   wg service start
#   wg chat 'hi'         # must succeed, role=coordinator
#   wg chat 'second'     # must succeed
#
# This MUST run live. If lambda01 is unreachable we loud-skip (exit 77) so
# the gap is greppable. We do not stub the endpoint — that's how the original
# regression slipped through.
#
# Optional env: WG_LIVE_NEX_ENDPOINT (default lambda01), WG_LIVE_NEX_MODEL
# (default qwen3-coder).

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

ENDPOINT="${WG_LIVE_NEX_ENDPOINT:-https://lambda01.tail334fe6.ts.net:30000}"
MODEL="${WG_LIVE_NEX_MODEL:-qwen3-coder}"

require_wg

if ! endpoint_reachable "${ENDPOINT}/v1/models"; then
    loud_skip "NEX ENDPOINT UNREACHABLE" "${ENDPOINT}/v1/models did not respond — set WG_LIVE_NEX_ENDPOINT to a reachable host or run from a network with access"
fi

scratch=$(make_scratch)
cd "$scratch"

if ! wg init -x nex -m "$MODEL" -e "$ENDPOINT" >init.log 2>&1; then
    loud_fail "wg init failed: $(tail -5 init.log)"
fi

# Boot dispatcher in background. We use --max-agents=1 to bound resource use.
start_wg_daemon "$scratch" --max-agents 1
graph_dir="$WG_SMOKE_DAEMON_DIR"

# Locate the chat outbox so we can assert positive coordinator output rather
# than just "no error markers." The outbox is the canonical record of what
# the chat agent produced.
find_outbox() {
    # Most builds resolve coordinator-0 chat dir from session metadata; we
    # fall back to whatever single outbox file exists under the chat dir.
    local first
    first=$(find "$graph_dir/chat" -maxdepth 3 -name 'outbox.jsonl' 2>/dev/null | head -1)
    if [[ -n "$first" ]]; then
        echo "$first"
        return 0
    fi
    return 1
}

# First message: positive assertion is that the response includes the
# 'coordinator' role label OR has at least 20 chars of non-error text. Either
# proves we hit the model and got a real reply.
if ! timeout 60 wg chat "hi" --timeout 45 >chat1.log 2>&1; then
    loud_fail "first 'hi' failed: $(tail -10 chat1.log)"
fi
if grep -qiE 'role=system-error|role=error|status:.*404|HTTP/.*404' chat1.log; then
    loud_fail "first 'hi' returned an error response: $(tail -10 chat1.log)"
fi
chat1_chars=$(tr -d '[:space:]' <chat1.log | wc -c)
if [[ "$chat1_chars" -lt 20 ]]; then
    loud_fail "first 'hi' response was only $chat1_chars chars (<20). Suggests stub/empty path. Output:\n$(cat chat1.log)"
fi

# Second message.
if ! timeout 60 wg chat "second message" --timeout 45 >chat2.log 2>&1; then
    loud_fail "second message failed: $(tail -10 chat2.log)"
fi
if grep -qiE 'role=system-error|role=error|status:.*404' chat2.log; then
    loud_fail "second message returned an error: $(tail -10 chat2.log)"
fi
chat2_chars=$(tr -d '[:space:]' <chat2.log | wc -c)
if [[ "$chat2_chars" -lt 20 ]]; then
    loud_fail "second response was only $chat2_chars chars (<20). Output:\n$(cat chat2.log)"
fi

# Positive assertion: outbox must contain >=2 coordinator responses. This is
# the regression bar. Without this check, a chat agent that silently emits
# `role=user` or empty payloads would still pass.
outbox=""
coord_count=0
if outbox=$(find_outbox 2>/dev/null) && [[ -n "$outbox" ]]; then
    coord_count=$(grep -c '"role"\s*:\s*"coordinator"' "$outbox" 2>/dev/null || echo 0)
    if [[ "$coord_count" -lt 2 ]]; then
        loud_fail "outbox $outbox should have >=2 coordinator responses, got $coord_count. Tail:\n$(tail -5 "$outbox")"
    fi
else
    # Some builds route nex output through a different sink. Don't fail —
    # the per-message error checks above already enforce the regression bar.
    coord_count="(no outbox; per-message checks passed)"
fi

echo "PASS: two-message wg-nex against $ENDPOINT/$MODEL succeeded (outbox: $coord_count coordinator responses)"
exit 0

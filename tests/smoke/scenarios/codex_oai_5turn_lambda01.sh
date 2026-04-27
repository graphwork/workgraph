#!/usr/bin/env bash
# Scenario: codex_oai_5turn_lambda01
#
# Reproduces the user's verbatim repro for the codex thin-wrapper executor:
#
#   $ cd $(mktemp -d)
#   $ wg init -m qwen3-coder -e https://lambda01.tail334fe6.ts.net:30000 -x codex
#   $ wg service start
#   $ wg tui          # send 5 messages back-to-back
#   → all 5 must produce coordinator responses (no fault on turn 2 — the
#     original wg-nex pain that this thin-wrapper path is meant to avoid).
#
# The codex CLI is a thin wrapper over the OAI Responses API. lambda01-style
# endpoints (sglang/vLLM) implement /v1/responses, so codex_handler.rs's
# `--config model_providers.wg.base_url=...` plumbing should route every turn
# to the configured endpoint. This scenario asserts that end-to-end.
#
# Pre-conditions surfaced as loud SKIP (exit 77) — never silently:
#   - codex CLI not installed (gate per task spec)
#   - lambda01 endpoint /v1/models unreachable
#   - lambda01 endpoint does NOT speak the OAI Responses API
#
# Behavioral assertions (the regression bar — not just exit codes):
#   - Outbox contains >=5 'coordinator' role messages
#   - Each of the 5 has non-empty content
#   - All 5 request_ids are distinct (no silent caching / dedup)
#
# Optional env:
#   WG_LIVE_NEX_ENDPOINT  default https://lambda01.tail334fe6.ts.net:30000
#   WG_LIVE_NEX_MODEL     default qwen3-coder

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

ENDPOINT="${WG_LIVE_NEX_ENDPOINT:-https://lambda01.tail334fe6.ts.net:30000}"
MODEL="${WG_LIVE_NEX_MODEL:-qwen3-coder}"

require_wg

if ! command -v codex >/dev/null 2>&1; then
    loud_skip "CODEX CLI MISSING" "codex binary not on PATH; install codex-cli to exercise the thin-wrapper smoke (task: thin-wrapper-smoke)"
fi

if ! endpoint_reachable "${ENDPOINT}/v1/models"; then
    loud_skip "CODEX ENDPOINT UNREACHABLE" "${ENDPOINT}/v1/models did not respond — set WG_LIVE_NEX_ENDPOINT to a reachable host"
fi

# codex 0.120+ requires the OAI Responses API; chat completions is no longer
# supported (openai/codex#7782). If the endpoint doesn't accept POST
# /v1/responses we loud-skip — failing here would surface a wrapper-target
# limitation, not a wg thin-wrapper regression.
if ! curl -fsS -k -m 8 -o /dev/null \
        -H 'Content-Type: application/json' \
        -d '{"model":"'"$MODEL"'","input":"hi"}' \
        "${ENDPOINT}/v1/responses" 2>/dev/null; then
    loud_skip "CODEX RESPONSES API UNAVAILABLE" "${ENDPOINT}/v1/responses rejected probe; codex requires the Responses API"
fi

scratch=$(make_scratch)
trap 'rm -rf "$scratch"' EXIT
cd "$scratch"

# codex looks up its bearer token via the env var named in the provider's
# `env_key` (OPENAI_API_KEY by codex_oai_compat::ENV_KEY_NAME). lambda01 does
# not validate auth, but codex still wants a non-empty value to construct the
# Authorization header.
export OPENAI_API_KEY="${OPENAI_API_KEY:-dummy-codex-smoke-key}"

if ! wg init -m "$MODEL" -e "$ENDPOINT" -x codex >init.log 2>&1; then
    loud_fail "wg init failed: $(tail -10 init.log)"
fi

# Discover the canonical workgraph dir (.wg or legacy .workgraph).
graph_dir=""
for cand in .wg .workgraph; do
    if [[ -d "$scratch/$cand" ]]; then
        graph_dir="$scratch/$cand"
        break
    fi
done
if [[ -z "$graph_dir" ]]; then
    loud_fail "no .wg/ or .workgraph/ directory after init"
fi

# Boot dispatcher in the background. --max-agents 1 keeps resource use bounded
# and forces serial chat-handler activity (matches user's TUI usage).
wg service start --max-agents 1 >daemon.log 2>&1 &
daemon_pid=$!
trap 'kill_tree "$daemon_pid"; rm -rf "$scratch"' EXIT

ready=false
for _ in $(seq 1 30); do
    if [[ -S "$graph_dir/service/daemon.sock" ]] || [[ -f "$graph_dir/service/state.json" ]]; then
        ready=true
        break
    fi
    sleep 0.5
done
if ! $ready; then
    loud_fail "daemon did not come up within 15s. Tail of daemon.log:\n$(tail -10 daemon.log)"
fi

# Mirror the user's TUI repro programmatically: in `wg tui` the chat session
# is created when the user opens the new-chat dialog (`+` key). For a scripted
# smoke we use the IPC entry point — `wg service create-chat` — which creates
# the `.chat-0` task that the supervisor then picks up. (Pure-IPC `wg chat 'hi'`
# alone lazy-spawns the supervisor, but the supervisor exits as orphan when no
# `.chat-N` task exists in the graph; this matches the daemon's "Use `wg chat
# new` (or the TUI '+' key)" guidance line.)
if ! wg service create-chat >create-chat.log 2>&1; then
    loud_fail "wg service create-chat failed: $(cat create-chat.log)"
fi
# Give the supervisor a moment to spawn the codex_handler.
sleep 3

# Send 5 messages back-to-back. Each message has a distinct payload so the
# scenario can verify the coordinator picks up each one (no silent dedup) and
# the codex_handler keeps working past turn 1 (the original nex regression).
#
# The first `wg chat` invocation against a fresh init also triggers the
# coordinator-0 chat session creation (no chat-loop tasks are seeded by
# `wg init` anymore — the daemon spawns the supervisor on first message).
for n in 1 2 3 4 5; do
    log="$scratch/chat$n.log"
    echo "  [turn $n] sending..." >&2
    if ! timeout 120 wg chat "Smoke turn $n: please reply with the word OK and nothing else." --timeout 90 >"$log" 2>&1; then
        loud_fail "turn $n failed (regression: codex thin-wrapper must not break after turn $((n-1))). Tail:\n$(tail -15 "$log")\nDaemon tail:\n$(tail -15 "$scratch/daemon.log" 2>/dev/null)"
    fi
    if grep -qiE 'role=system-error|role=error|status:.*404|HTTP/.*404' "$log"; then
        loud_fail "turn $n produced an error response: $(tail -10 "$log")"
    fi
    echo "  [turn $n] $(tr -d '[:space:]' <"$log" | wc -c) chars in response" >&2
done

# Locate the chat outbox. coordinator-0 is the only chat session in this
# scenario; we pick the first non-empty outbox.jsonl under chat/.
outbox=""
while IFS= read -r cand; do
    if [[ -s "$cand" ]]; then
        outbox="$cand"
        break
    fi
done < <(find "$graph_dir/chat" -maxdepth 3 -name 'outbox.jsonl' 2>/dev/null)

if [[ -z "$outbox" ]]; then
    loud_fail "no non-empty outbox.jsonl under $graph_dir/chat (scratch preserved at $scratch — but trap rm -rf will fire)"
fi

# Behavioral assertions: parse outbox.jsonl and check the regression bar.
# Stay strict — empty content or duplicate request_ids are the silent failure
# modes that this scenario exists to catch.
python3 - "$outbox" <<'PY' || loud_fail "outbox behavioral assertions failed"
import json, sys

path = sys.argv[1]
coord_msgs = []
with open(path) as fh:
    for line in fh:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except Exception:
            continue
        if msg.get("role") == "coordinator":
            coord_msgs.append(msg)

if len(coord_msgs) < 5:
    print(
        f"FAIL: expected >=5 'coordinator' outbox responses, got {len(coord_msgs)}",
        file=sys.stderr,
    )
    for m in coord_msgs[-3:]:
        print(
            f"  tail: id={m.get('id')} req={m.get('request_id')!r} "
            f"content[:80]={m.get('content','')[:80]!r}",
            file=sys.stderr,
        )
    sys.exit(1)

last5 = coord_msgs[-5:]

empty = [m for m in last5 if not (m.get("content") or "").strip()]
if empty:
    print(
        f"FAIL: {len(empty)} of the last 5 coordinator responses had empty content",
        file=sys.stderr,
    )
    for m in empty:
        print(f"  empty: id={m.get('id')} req={m.get('request_id')!r}", file=sys.stderr)
    sys.exit(1)

req_ids = [m.get("request_id") for m in last5]
if any(not r for r in req_ids):
    print(f"FAIL: missing request_id on at least one of the last 5 responses: {req_ids}", file=sys.stderr)
    sys.exit(1)
if len(set(req_ids)) != 5:
    print(
        f"FAIL: last 5 coordinator responses had non-distinct request_ids: {req_ids}",
        file=sys.stderr,
    )
    sys.exit(1)

print(f"OK: 5 distinct coordinator responses with non-empty content; req_ids={req_ids}")
PY

echo "PASS: 5-turn wg-codex-handler against ${ENDPOINT}/${MODEL} succeeded"
exit 0

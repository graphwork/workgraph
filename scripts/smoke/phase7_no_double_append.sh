#!/usr/bin/env bash
# Phase 7 regression smoke: the daemon must NOT double-append user
# chat messages to the inbox regardless of executor.
#
# Before the fix, `CoordinatorAgent::uses_subprocess` only returned
# true for `executor=native` (or oai-compat/openrouter providers);
# claude and codex coordinators fell into the inline send-message
# path which forwarded through the mpsc rx → `append_inbox_for`
# even though the IPC UserChat handler ALREADY appended. Result:
# every user turn landed in `inbox.jsonl` twice with the same
# `request_id`, and the coordinator complained about "looping
# messages" because its inbox literally had duplicates.
#
# This smoke sends one user turn via `wg chat`, waits for the
# daemon's settling tick to elapse, then asserts the inbox has
# EXACTLY ONE row with that request_id. Runs against all three
# Phase 7 executors.
set -euo pipefail
tmp=$(mktemp -d)
cleanup() {
  for p in $(pgrep -x wg 2>/dev/null); do
    c=$(cat /proc/$p/comm 2>/dev/null); [ "$c" = "wg" ] && kill "$p" 2>/dev/null
  done
  rm -rf "$tmp" 2>/dev/null || true
}
trap cleanup EXIT
cd "$tmp"
export WG_DIR="$tmp/.workgraph"
wg init --no-agency >/dev/null 2>&1
# Disable agency scaffolding so its tasks don't compete for slots.
for flag in auto-evaluate flip-enabled auto-assign auto-place auto-create auto-triage; do
  wg config --"$flag" false >/dev/null 2>&1 || true
done

fail() { echo "FAIL: $*"; exit 1; }
pass() { echo "PASS: $*"; }

# Assert: the single request_id `$2` appears exactly ONCE in
# `coordinator-$1`'s inbox.jsonl. Before the fix: 2.
assert_single_inbox_row() {
  local cid=$1 req=$2 label=$3
  local inbox="$WG_DIR/chat/coordinator-$cid/inbox.jsonl"
  local count
  count=$(jq -r --arg r "$req" 'select(.request_id == $r) | .id' "$inbox" 2>/dev/null | wc -l)
  if [ "$count" = "1" ]; then
    pass "$label: inbox has exactly 1 row for request_id=$req"
  else
    echo "DIAG — all rows with that request_id:"
    jq -c --arg r "$req" 'select(.request_id == $r) | {id, timestamp, content: .content[0:30]}' "$inbox"
    fail "$label: expected 1 inbox row for $req, got $count"
  fi
}

test_executor() {
  local label=$1 executor=$2 extra_model=${3:-}
  echo "=== $label ($executor) ==="
  wg config --coordinator-executor "$executor" >/dev/null 2>&1
  if [ -n "$extra_model" ]; then
    wg config --coordinator-model "$extra_model" >/dev/null 2>&1
  fi
  wg service start >/dev/null 2>&1 &
  sleep 4
  wg service status 2>&1 | grep -qE "^Service: running" || fail "daemon not running"

  # Send exactly ONE message. We don't care what the coordinator
  # replies — only that the inbox gets a single row.
  wg chat --coordinator 0 "ping-$label" --timeout 30 >/dev/null 2>&1 &
  local chat_pid=$!

  # Wait for the inbox to receive at least one row.
  local inbox="$WG_DIR/chat/coordinator-0/inbox.jsonl"
  for i in {1..60}; do
    [ -f "$inbox" ] && [ -s "$inbox" ] && break
    sleep 0.5
  done
  [ -s "$inbox" ] || fail "$label: inbox never received a message"

  # Sleep past the daemon's settling_delay_ms (default 2000) +
  # one full coordinator tick so route_chat_to_agent has had a
  # chance to run. Pre-fix this was the exact window in which the
  # second append would land.
  sleep 5

  # Extract the single user request_id from the inbox. Filter
  # out heartbeats so a slow first tick doesn't fold them in.
  local req
  req=$(jq -r 'select(.content | startswith("ping-")) | .request_id' "$inbox" | head -1)
  [ -n "$req" ] || fail "$label: no user row in inbox"
  assert_single_inbox_row 0 "$req" "$label"

  wg service stop >/dev/null 2>&1 || true
  kill "$chat_pid" 2>/dev/null || true
  wait "$chat_pid" 2>/dev/null || true
  # Tear down lingering handler processes + reset coord state.
  for p in $(pgrep -x wg 2>/dev/null); do
    c=$(cat /proc/$p/comm 2>/dev/null); [ "$c" = "wg" ] && kill "$p" 2>/dev/null
  done
  sleep 1
  rm -rf "$WG_DIR/chat/coordinator-0" "$WG_DIR/chat/0" 2>/dev/null
  # Remove state so next iteration starts fresh.
  rm -f "$WG_DIR/service/coordinator-state-0.json" 2>/dev/null
  echo
}

test_executor claude claude
test_executor codex  codex
test_executor native native oai-compat:qwen3-coder-30b

echo "=== ALL PHASE 7 NO-DOUBLE-APPEND CHECKS PASSED ==="

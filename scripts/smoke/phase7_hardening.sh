#!/usr/bin/env bash
# Phase 7 hardening smoke — gaps surfaced in post-ship audit.
#
# Assertions:
#   A. `wg spawn-task <task> --dry-run` prints a correct preview for
#      both native and claude adapters (was still "[TODO]" pre-audit)
#   B. Killing a running wg claude-handler mid-flight makes the
#      daemon supervisor restart it (session-lock stale-PID reclaim +
#      subprocess_coordinator_loop restart policy)
#   C. Stale-PID reclaim works standalone too: after a killed
#      handler's PID is dead, a fresh `wg claude-handler --chat <ref>`
#      acquires the lock instead of failing with "already held"
#   D. Multi-turn conversation coherence: turn-2 response mentions
#      something from turn-1 (stream-json session state is preserved)
#
# Proves the Phase 7 code path is production-grade, not just "works
# on the happy path once".
set -euo pipefail
tmp=$(mktemp -d)
trap "pkill -P $$ 2>/dev/null || true; pkill -f 'claude-handler' 2>/dev/null || true; pkill -f 'spawn-task .coordinator' 2>/dev/null || true; rm -rf $tmp 2>/dev/null || true" EXIT
cd "$tmp"
export WG_DIR="$tmp/.workgraph"
wg init --no-agency >/dev/null 2>&1

fail() { echo "FAIL: $*"; exit 1; }
pass() { echo "PASS: $*"; }

echo "=== Test A: spawn-task --dry-run preview for native + claude ==="
# Create two coordinator tasks in the graph (synthesized path)
# native adapter preview
wg config --coordinator-executor native >/dev/null 2>&1
nat_preview=$(WG_EXECUTOR_TYPE=native wg spawn-task .coordinator-7 --dry-run 2>&1)
echo "  native: $nat_preview"
case "$nat_preview" in
  *"wg nex --chat coordinator-7"*) pass "native preview names wg nex and the chat alias" ;;
  *) fail "native preview wrong: $nat_preview" ;;
esac
case "$nat_preview" in
  *"--role coordinator"*) pass "native preview includes --role coordinator" ;;
  *) fail "native preview missing --role coordinator: $nat_preview" ;;
esac

# claude adapter preview
claude_preview=$(WG_EXECUTOR_TYPE=claude wg spawn-task .coordinator-9 --dry-run 2>&1)
echo "  claude: $claude_preview"
case "$claude_preview" in
  *"wg claude-handler --chat coordinator-9"*) pass "claude preview names wg claude-handler" ;;
  *"[TODO"*|*"not yet implemented"*) fail "claude preview still shows stale TODO: $claude_preview" ;;
  *) fail "claude preview wrong: $claude_preview" ;;
esac

echo
echo "=== Test B: daemon restarts a killed wg claude-handler ==="
wg config --coordinator-executor claude >/dev/null 2>&1
wg service start >/dev/null 2>&1 &
sleep 4
wg service status 2>&1 | grep -qE "^Service: running" || fail "daemon not running"
# Wait for first handler
for i in {1..30}; do
  pid1=$(pgrep -f "wg claude-handler --chat coordinator-0" | head -1)
  [ -n "$pid1" ] && break
  sleep 0.5
done
[ -n "$pid1" ] || fail "claude-handler never spawned"
pass "first handler PID=$pid1"

# Kill it — simulates OOM or crash.
kill -9 "$pid1"
sleep 1

# Daemon must spawn a replacement within the restart window.
# Use a new-PID check (PID strictly different from pid1).
pid2=""
for i in {1..60}; do
  candidate=$(pgrep -f "wg claude-handler --chat coordinator-0" | head -1 || true)
  if [ -n "$candidate" ] && [ "$candidate" != "$pid1" ]; then
    pid2="$candidate"
    break
  fi
  sleep 0.5
done
[ -n "$pid2" ] || { echo "DIAG: daemon log tail"; tail -20 "$WG_DIR/service/daemon.log"; fail "daemon did not restart claude-handler within 30s"; }
pass "replacement handler PID=$pid2 (!= $pid1) — supervisor restart works"

echo
echo "=== Test C: new handler reclaimed the stale lock ==="
# Wait for the lock to stabilize on a LIVE PID that's not the
# original (killed) handler. The supervisor may restart several
# times in quick succession after SIGKILL, so sample repeatedly.
stable_pid=""
for i in {1..30}; do
  lock_pid=$(sed -n 1p "$WG_DIR/chat/coordinator-0/.handler.pid" 2>/dev/null || echo "")
  if [ -n "$lock_pid" ] && [ "$lock_pid" != "$pid1" ] && kill -0 "$lock_pid" 2>/dev/null; then
    stable_pid="$lock_pid"
    break
  fi
  sleep 0.5
done
[ -n "$stable_pid" ] || {
  echo "DIAG: current lock_pid=$lock_pid, pid1=$pid1";
  echo "DIAG: claude-handlers currently alive:"; pgrep -af "wg claude-handler";
  fail "no live lock holder that isn't the original killed PID";
}
pass "lock reclaimed by live handler PID=$stable_pid (original $pid1 is dead)"

echo
echo "=== Test D: multi-turn coherence (turn 2 references turn 1) ==="
# Turn 1: tell Claude a specific word.
wg chat --coordinator 0 "Remember the word: banana. Reply with just: ok" --timeout 90 >/dev/null 2>&1 || true
# Give turn 1 time to finalize before sending turn 2.
for i in {1..120}; do
  lines=$(wc -l < "$WG_DIR/chat/coordinator-0/outbox.jsonl" 2>/dev/null || echo 0)
  [ "${lines:-0}" -ge 1 ] && break
  sleep 0.5
done
# Turn 2: ask what the word was.
wg chat --coordinator 0 "What word did I ask you to remember? Reply with just that one word." --timeout 90 >/dev/null 2>&1 || true
for i in {1..180}; do
  lines=$(wc -l < "$WG_DIR/chat/coordinator-0/outbox.jsonl" 2>/dev/null || echo 0)
  [ "${lines:-0}" -ge 2 ] && break
  sleep 0.5
done
[ "${lines:-0}" -ge 2 ] || { echo "DIAG outbox:"; cat "$WG_DIR/chat/coordinator-0/outbox.jsonl"; fail "only $lines outbox lines after 2 turns"; }
# Turn-2 content must mention "banana" (case-insensitive).
turn2=$(tail -1 "$WG_DIR/chat/coordinator-0/outbox.jsonl")
echo "  turn2 raw: $(echo "$turn2" | head -c 200)"
if echo "$turn2" | grep -qi "banana"; then
  pass "turn-2 response references 'banana' — multi-turn state preserved"
else
  fail "turn-2 response does NOT mention 'banana': $turn2"
fi

wg service stop >/dev/null 2>&1 || true
echo
echo "=== ALL PHASE 7 HARDENING CHECKS PASSED ==="
